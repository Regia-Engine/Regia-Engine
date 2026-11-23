// SPDX-License-Identifier: MIT-0
//! `sim_bridge` (M3 mock hover, M9 framework slice): three hover discs on a heightfield with the
//! full sim bridge, presentation scripts, chase/overhead/free cameras, timers, tweens and live
//! settings (`docs/specs/14` M9, `specs/25` § 3).
//! Touch controls on Android (see [`touch`]) and a one-shot sound on the first sim event (M3).
//!
//! # How the pieces fit
//!
//! [`HoverSim`] is the authoritative simulation — it owns its tick clock and emits a
//! [`HoverSnapshot`] of three `"disc"` entities each frame through the `ExternalSim` trait.
//! [`SimBridgeSample`] wires it into a `SimBridgePlugin`, then attaches presentation (a chase
//! [`CameraRig`], [`DiscPresenter`] scripts reading sim extras) and the render side (heightfield +
//! disc meshes, a sun). This sample is the reference for how DO27's shell drives a real sim.
//!
//! Presentation physics next to the sim (kinematic hulls, terrain collider, debris) calls
//! `regia_framework::sim_bridge::presentation` from [`presentation_physics`]
//! (`--presentation-physics` in the window).
//!
//! See `book/agents/gameplay.md` and `docs/specs/25-sim-bridge-and-do27.md`.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Easing, Quat, Vec3};
use regia_platform::{ButtonState, KeepScreenOn, KeyboardInput, TouchInput, Vfs, VfsPath};
use tracing::{info, warn};

use regia_audio::prelude::*;
use regia_framework::camera::{
    CameraMode, CameraPlugin, CameraRig, ChaseCamera, FreeLookInput, Overhead, ThirdPerson,
};
use regia_framework::script::{RegisterScriptExt, Script, ScriptCtx, ScriptPlugin};
use regia_framework::settings::{GameSettings, SettingsChanged, SettingsPlugin};
use regia_framework::sim_bridge::{
    BridgeInput, EventRouter, ExternalSim, KindRegistry, SimBridgePlugin, SimEntityView, SimEvent, SimExtras,
    SimSnapshot, SnapshotTick, ToastRaised,
};
use regia_framework::timers::{TimerPlugin, schedule_after};
use regia_framework::tween::{Tween, TweenPlugin};
use regia_input::prelude::*;
use regia_render::{Camera, CpuMaterial, CpuMesh, DirectionalLight, MeshRenderer};
use regia_scene::{GlobalTransform, Transform};

pub mod presentation_physics;
pub mod touch;
use touch::{TouchControls, update_touch_controls};

// ---------------------------------------------------------------------------
// Mock hover sim
// ---------------------------------------------------------------------------

/// One entry in the hover snapshot.
#[derive(Debug, Clone)]
struct HoverEntry {
    id: u64,
    kind: String,
    team: u32,
    translation: Vec3,
    rotation: Quat,
    /// Presentation extra read by [`DiscPresenter`].
    lean: f32,
}

/// A snapshot holding a flat list of entities.
#[derive(Default)]
pub struct HoverSnapshot {
    entries: Vec<HoverEntry>,
}

impl SimSnapshot for HoverSnapshot {
    fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
        for entry in &self.entries {
            f(SimEntityView {
                id: entry.id,
                kind: &entry.kind,
                team: entry.team,
                translation: entry.translation,
                rotation: entry.rotation,
            });
        }
    }

    fn extras_for(&self, id: u64, out: &mut Vec<(String, f32)>) {
        if let Some(entry) = self.entries.iter().find(|e| e.id == id) {
            out.push(("lean".to_string(), entry.lean));
        }
    }
}

/// The authoritative hover sim: three discs (ids 1, 2, 3), kind `"disc"`.
///
/// Each frame disc *i* sits at `(i as f32 * 2.0, 1.5, 0.0)` plus a step along X of
/// `tick * 0.01`. The sim owns the tick counter. `submit_input` records the last
/// [`BridgeInput`] but does not move the discs — authority stays with the sim.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use sim_bridge::{HoverSim, HoverSnapshot};
/// use regia_framework::sim_bridge::{ExternalSim, SimSnapshot};
///
/// let mut sim = HoverSim::new();
/// let mut snapshot = HoverSnapshot::default();
/// let tick = sim.read_snapshot(Duration::ZERO, &mut snapshot);
/// assert_eq!(tick.0, 1);
/// let mut count = 0;
/// snapshot.for_each_entity(|e| {
///     assert_eq!(e.kind, "disc");
///     count += 1;
/// });
/// assert_eq!(count, 3); // three hover discs
/// ```
#[derive(Resource)]
pub struct HoverSim {
    /// Monotonic frame counter (the sim's own clock).
    tick: u64,
    /// The three disc ids.
    disc_ids: Vec<u64>,
    /// Most recent input received from Regia Engine.
    last_input: Option<BridgeInput>,
    /// Whether the first-set events have been sent.
    events_drained: bool,
}

impl Default for HoverSim {
    fn default() -> Self {
        Self::new()
    }
}

impl HoverSim {
    /// Creates the sim with three discs and one queued event.
    pub fn new() -> Self {
        Self {
            tick: 0,
            disc_ids: vec![1, 2, 3],
            last_input: None,
            events_drained: false,
        }
    }

    /// World-space position of disc `id` at the given sim tick.
    fn disc_translation(id: u64, tick: u64) -> Vec3 {
        Vec3::new(id as f32 * 2.0 + tick as f32 * 0.01, 1.5, 0.0)
    }
}

impl ExternalSim for HoverSim {
    type Snapshot = HoverSnapshot;
    type Event = String;

    fn submit_input(&mut self, input: &BridgeInput) {
        self.last_input = Some(input.clone());
    }

    fn read_snapshot(&mut self, _frame_time: Duration, out: &mut Self::Snapshot) -> SnapshotTick {
        self.tick += 1;
        let tick = self.tick;
        out.entries.clear();
        for &id in &self.disc_ids {
            out.entries.push(HoverEntry {
                id,
                kind: "disc".into(),
                team: 0,
                translation: Self::disc_translation(id, tick),
                rotation: Quat::IDENTITY,
                lean: ((id as f32 * 0.7 + tick as f32 * 0.02).sin() * 0.25).clamp(-0.5, 0.5),
            });
        }
        SnapshotTick(tick)
    }

    fn drain_events(&mut self, out: &mut Vec<Self::Event>) {
        if !self.events_drained {
            out.push("sim_started".into());
            self.events_drained = true;
        }
    }

    fn event_kind(event: &Self::Event) -> String {
        event.clone()
    }
}

// ---------------------------------------------------------------------------
// Presentation script + kind registry (M9)
// ---------------------------------------------------------------------------

/// Records presentation-script updates for tests.
#[derive(Resource, Default)]
pub struct PresenterLog(pub Vec<String>);

/// Reads sim extras and drives chase lean on the look-at target (M9 acceptance).
#[derive(regia_macros::Script, Reflect, Default, Clone)]
#[script(order = 0)]
pub struct DiscPresenter {
    lean_seen: f32,
}

impl Script for DiscPresenter {
    fn update(&mut self, ctx: &mut ScriptCtx) {
        let lean = ctx
            .get::<SimExtras>()
            .and_then(|e| e.0.iter().find(|(n, _)| n == "lean").map(|(_, v)| *v))
            .unwrap_or(0.0);
        self.lean_seen = lean;
        if let Some(mut log) = ctx.world().get_resource_mut::<PresenterLog>() {
            log.0.push(format!("presenter:lean={lean:.3}"));
        }
    }
}

fn disc_kinds() -> KindRegistry {
    KindRegistry::from_ron(
        r#"(kinds: {
            "disc": (prefab: None,
                     presentation_scripts: ["sim_bridge::DiscPresenter"]),
        })"#,
    )
    .expect("valid kinds.ron for sim_bridge")
}

fn sim_event_router() -> EventRouter {
    EventRouter::from_ron(r#"(routes: { "sim_started": [ Toast(text: "Sim online") ] })"#)
        .expect("valid event router for sim_bridge")
}

/// Tracks M9 demo systems for integration tests.
#[derive(Resource, Default)]
pub struct M9DemoState {
    /// One-shot timer from `schedule_m9_demo` fired.
    pub timer_fired: bool,
    /// Event router raised a toast for `sim_started`.
    pub toast_seen: bool,
    /// Live settings change (audio.master → 0.8) was observed.
    pub settings_live: bool,
}

// ---------------------------------------------------------------------------
// Sample plugin
// ---------------------------------------------------------------------------

/// The `sim_bridge` sample: a mock hover sim with a chase camera and audio.
pub struct SimBridgeSample;

/// Exit after this many frames for automated smoke runs.
#[derive(Resource, Debug, Clone, Copy)]
pub struct ExitAfterFrames(pub u64);

impl Plugin for SimBridgeSample {
    fn build(&self, app: &mut App) {
        let config = InputConfig::new()
            .action("Move", ActionKind::Axis2D)
            .action("Look", ActionKind::Axis2D)
            .binding(Binding::new(
                "Move",
                InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
            ))
            .binding(Binding::new("Look", InputSource::MouseDelta));

        app.add_plugins((
            SimBridgePlugin::<HoverSim>::new(HoverSim::new())
                .kinds(disc_kinds())
                .events(sim_event_router()),
            ScriptPlugin,
            TimerPlugin,
            TweenPlugin,
            SettingsPlugin,
            CameraPlugin,
            InputPlugin::new(config),
            regia_framework::startup_playback::StartupPlaybackPlugin,
        ))
        .add_plugins((AudioPlugin, regia_scene::ScenePlugin))
        .register_script::<DiscPresenter>()
        .add_message::<TouchInput>()
        .init_resource::<TouchControls>()
        .init_resource::<PresenterLog>()
        .init_resource::<M9DemoState>()
        // Periodic fps/frame-time line in the log — the "is it rendering" evidence of smoke runs.
        .insert_resource(regia_app::DiagnosticsConfig {
            log_interval: Some(Duration::from_secs(2)),
            ..Default::default()
        })
        .add_systems(
            Startup,
            (
                spawn_camera_and_target,
                spawn_scene_entities,
                schedule_m9_demo,
                keep_screen_on,
                log_about,
            ),
        )
        .add_systems(PreUpdate, update_touch_controls)
        .add_systems(
            Update,
            (
                write_move_input,
                write_free_look_input,
                write_look_input,
                camera_mode_hotkeys,
                track_m9_demo,
                on_first_sim_event,
                exit_after_frames,
            ),
        )
        .add_systems(PostUpdate, attach_disc_meshes);
    }
}

/// Keep the display awake while the slice runs (exercises `KeepScreenOn` on Android). Absent
/// without `PlatformPlugin` (headless tests), where there is no display to keep awake.
fn keep_screen_on(mut keep: Option<ResMut<KeepScreenOn>>) {
    if let Some(keep) = keep.as_mut() {
        keep.0 = true;
    }
}

/// Reads the shipped `about.txt` through `assets://` at startup and logs its first line.
///
/// On Android the file lives inside the APK and resolves through the VFS APK mount, so this line
/// in logcat is the evidence that packaged assets reach the sample. A missing or unreadable file
/// degrades to a warning, never a panic.
fn log_about(vfs: Option<Res<Vfs>>) {
    let Some(vfs) = vfs else {
        warn!("sim_bridge: no Vfs mounted; skipping assets://about.txt");
        return;
    };
    let path = match VfsPath::parse("assets://about.txt") {
        Ok(path) => path,
        Err(err) => {
            warn!("sim_bridge: assets://about.txt is not a valid VFS path: {err}");
            return;
        }
    };
    match vfs.read_sync(&path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let first = text.lines().next().unwrap_or_default();
            info!("sim_bridge: {first}");
        }
        Err(err) => warn!("sim_bridge: cannot read assets://about.txt: {err}"),
    }
}

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

/// Copies the `"Move"` action value plus the touch stick into [`BridgeInput::move_axes`] so the
/// sim receives it through `submit_input` the following frame. Keyboard and touch add up; the
/// combined vector is clamped to the unit circle (like `keys2d` normalises diagonals).
fn write_move_input(actions: Res<Actions>, touch: Res<TouchControls>, mut bridge_input: ResMut<BridgeInput>) {
    let combined = actions.axis2d("Move") + touch.move_axes();
    bridge_input.move_axes = touch::clamp_to_unit_circle(combined);
}

/// Drives the chase camera's yaw and pitch from the `"Look"` action (mouse delta) plus the touch
/// look drag (right half of the screen).
///
/// Yaw is updated from `Look.x` and pitch from `Look.y` using a sensitivity of
/// [`touch::LOOK_RADIANS_PER_PIXEL`] rad/pixel (scaled by the sensitivity setting) from the
/// mouse and the touch look stick. Headless, both are zero, which keeps the camera behind the discs on +Z.
fn write_look_input(
    actions: Res<Actions>,
    settings: Res<GameSettings>,
    touch: Res<TouchControls>,
    mut cameras: Query<&mut CameraRig>,
) {
    let look = actions.axis2d("Look") + touch.look_delta();
    let scale = settings.input.sensitivity * touch::LOOK_RADIANS_PER_PIXEL;
    for mut rig in &mut cameras {
        if matches!(rig.mode, CameraMode::Free(_)) {
            continue;
        }
        rig.yaw -= look.x * scale;
        rig.pitch += look.y * scale;
    }
}

/// Drives [`FreeLookInput`] while the rig is in spectator mode (WASD + mouse).
fn write_free_look_input(actions: Res<Actions>, mut free: ResMut<FreeLookInput>) {
    free.move_axes = actions.axis2d("Move");
    free.look = actions.axis2d("Look") * 0.003;
}

/// F1 chase, F2 overhead, F3 free (`specs/25` § 4 camera switching).
fn camera_mode_hotkeys(mut keys: MessageReader<KeyboardInput>, mut cameras: Query<&mut CameraRig>) {
    for key in keys.read() {
        if key.state != ButtonState::Pressed || key.repeat {
            continue;
        }
        let mode = match key.physical_key.as_str() {
            "F1" => Some(CameraMode::ThirdPerson(ThirdPerson {
                boom_length: 10.0,
                socket_offset: Vec3::ZERO,
                target_offset: Vec3::ZERO,
                lag_speed: 5.0,
                rotation_lag: 5.0,
                min_pitch: -80.0_f32.to_radians(),
                max_pitch: 80.0_f32.to_radians(),
                max_lean: 0.22,
                lean_gain: 0.14,
                lean_speed: 6.0,
                // This sample does not probe the boom; 0 disables the cast.
                probe_radius: 0.0,
            })),
            "F2" => Some(CameraMode::Overhead(Overhead::default())),
            "F3" => Some(CameraMode::Free(Default::default())),
            _ => None,
        };
        if let Some(mode) = mode {
            for mut rig in &mut cameras {
                rig.switch_to(mode);
            }
        }
    }
}

/// One-shot timer (audio volume), routed toasts and settings changes for M9 tests.
fn schedule_m9_demo(world: &mut World) {
    schedule_after(world, 0.05, None, |world| {
        if let Some(mut settings) = world.get_resource_mut::<GameSettings>() {
            settings.audio.master = 0.8;
        }
        if let Some(mut demo) = world.get_resource_mut::<M9DemoState>() {
            demo.timer_fired = true;
        }
    });
}

fn track_m9_demo(
    mut demo: ResMut<M9DemoState>,
    mut toasts: MessageReader<ToastRaised>,
    mut settings_events: MessageReader<SettingsChanged>,
) {
    if toasts.read().next().is_some() {
        demo.toast_seen = true;
    }
    if settings_events.read().any(|e| e.after.audio.master == 0.8) {
        demo.settings_live = true;
    }
}

/// Plays a 16-sample headless-friendly one-shot the first time the sim emits an
/// event. Uses [`AudioSettings::headless`] in tests; the windowed binary uses the
/// default output device.
fn on_first_sim_event(
    mut events: MessageReader<SimEvent<String>>,
    mut audio: ResMut<Audio>,
    mut has_played: Local<bool>,
) {
    if *has_played {
        return;
    }
    // Drain all events this frame; play on the first one.
    if events.read().next().is_some() {
        if let Ok(clip) = SoundClip::new(vec![0.25; 16], 48_000) {
            let _ = audio.play(&clip);
        }
        *has_played = true;
    }
}

/// Shared scene meshes/materials for the ECS render path (uploaded once; the
/// renderer caches by `Arc` identity).
#[derive(Resource)]
pub struct SceneAssets {
    /// The flat heightfield (object space, centred at the origin).
    pub heightfield: std::sync::Arc<CpuMesh>,
    /// One hover disc (object space, centred at the origin).
    pub disc: std::sync::Arc<CpuMesh>,
    /// The warm-grey material every mesh uses.
    pub material: std::sync::Arc<CpuMaterial>,
}

/// `Startup`: spawn the heightfield entity and the sun light the runtime path
/// extracts each frame (the discs attach to the sim entities as they appear).
/// With `--presentation-physics` the ground is drawn from
/// [`presentation_physics::PresentationTerrain`], skipping the hole cells of the engine ground
/// collider built from the same map.
fn spawn_scene_entities(
    mut commands: Commands,
    terrain: Option<Res<presentation_physics::PresentationTerrain>>,
) {
    let (vertices, indices) = match terrain {
        Some(terrain) => terrain.mesh(),
        None => build_heightfield_mesh(),
    };
    let heightfield = std::sync::Arc::new(CpuMesh { vertices, indices });
    let disc = std::sync::Arc::new(CpuMesh {
        vertices: build_disc_mesh(0.5, 16).0,
        indices: build_disc_mesh(0.5, 16).1,
    });
    let material = std::sync::Arc::new(CpuMaterial {
        base_color: [0.6, 0.5, 0.4, 1.0],
        roughness: 0.7,
        ..CpuMaterial::default()
    });
    let transform = Transform::default();
    commands.spawn((
        regia_scene::components::Name::new("heightfield"),
        transform,
        GlobalTransform(transform.compute_affine()),
        MeshRenderer::new(
            std::sync::Arc::clone(&heightfield),
            std::sync::Arc::clone(&material),
        ),
    ));
    // Sun from the upper-right, physically bright (the post stack exposes).
    use regia_core::math::{Quat, Vec3};
    let dir = Vec3::new(-0.577, -0.577, -0.577).normalize_or_zero();
    let light_transform = Transform {
        translation: Vec3::ZERO,
        rotation: Quat::from_rotation_arc(-Vec3::Z, dir),
        scale: Vec3::ONE,
    };
    commands.spawn((
        regia_scene::components::Name::new("sun"),
        light_transform,
        GlobalTransform(light_transform.compute_affine()),
        DirectionalLight {
            direction: [-0.577, -0.577, -0.577],
            color: [1.0, 1.0, 1.0],
            illuminance: 100_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));
    commands.insert_resource(SceneAssets {
        heightfield,
        disc,
        material,
    });
}

/// `PostUpdate`: attach the disc mesh to every sim entity that does not have
/// one yet (the bridge spawns entities as the sim reports them).
fn attach_disc_meshes(
    mut commands: Commands,
    assets: Option<Res<SceneAssets>>,
    sim_entities: Query<(Entity, &regia_framework::sim_bridge::SimId), Without<MeshRenderer>>,
) {
    let Some(assets) = assets else {
        return;
    };
    for (entity, _) in &sim_entities {
        commands.entity(entity).insert(MeshRenderer::new(
            std::sync::Arc::clone(&assets.disc),
            std::sync::Arc::clone(&assets.material),
        ));
    }
}

/// Exits after `ExitAfterFrames` frames have elapsed.
fn exit_after_frames(limit: Option<Res<ExitAfterFrames>>, time: Res<Time>, mut exit: MessageWriter<AppExit>) {
    if let Some(limit) = limit
        && time.frame_count() >= limit.0
    {
        info!("sim_bridge: {} frames done, exiting", time.frame_count());
        exit.write(AppExit::Success);
    }
}

// ---------------------------------------------------------------------------
// Camera setup
// ---------------------------------------------------------------------------

/// Spawns the chase camera entity and the fixed look-at target it follows.
///
/// The target sits at the centroid of the three discs' initial positions so the
/// camera frames them from behind (+Z looking towards −Z).
fn spawn_camera_and_target(mut commands: Commands) {
    // The three discs start near (2, 1.5, 0), (4, 1.5, 0), (6, 1.5, 0).
    // Place the look-at target at their centroid.
    let target = commands
        .spawn((
            Transform {
                translation: Vec3::new(4.0, 1.5, 0.0),
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
            },
            Tween::translation_axis('y', 1.5, 2.25, 3.0, Easing::QuadOut),
        ))
        .id();

    let rig = CameraRig {
        mode: CameraMode::ThirdPerson(ThirdPerson {
            boom_length: 10.0,
            socket_offset: Vec3::new(0.0, 0.0, 0.0),
            target_offset: Vec3::new(0.0, 0.0, 0.0),
            lag_speed: 5.0,
            rotation_lag: 5.0,
            min_pitch: -80.0_f32.to_radians(),
            max_pitch: 80.0_f32.to_radians(),
            max_lean: 0.22,
            lean_gain: 0.14,
            lean_speed: 6.0,
            // This sample does not probe the boom; 0 disables the cast.
            probe_radius: 0.0,
        }),
        pitch: -0.15, // slight downward tilt
        yaw: 0.0,     // behind the target (camera on +Z, looking at −Z)
        ..CameraRig::default()
    };

    // The render camera joins the rig entity: the runtime path extracts
    // `Camera` + `GlobalTransform` (propagated by the scene plugin).
    commands.spawn((
        regia_scene::components::Name::new("main_camera"),
        Transform::default(),
        GlobalTransform::default(),
        rig,
        ChaseCamera { target },
        Camera::default(),
    ));
}

// ---------------------------------------------------------------------------
// Mesh builders (public so tests can use them)
// ---------------------------------------------------------------------------

/// Builds a flat 4×4 heightfield mesh centred at the origin, y = 0.
///
/// The grid spans from (-4, 0, -4) to (4, 0, 4) in object space (the entity's
/// world transform places it). Returns [`regia_render::LabVertex`] / index
/// data suitable for [`regia_render::CpuMesh`].
pub fn build_heightfield_mesh() -> (Vec<regia_render::LabVertex>, Vec<u32>) {
    let n = 5; // 5 vertices per side → 4 cells
    let half = 4.0_f32; // half-extent
    let cell = 2.0_f32; // cell size

    let mut vertices = Vec::with_capacity(n * n);
    for iz in 0..n {
        for ix in 0..n {
            let x = -half + ix as f32 * cell;
            let z = -half + iz as f32 * cell;
            vertices.push(regia_render::LabVertex {
                position: [x, 0.0, z],
                normal: [0.0, 1.0, 0.0],
                uv: [ix as f32 / (n - 1) as f32, iz as f32 / (n - 1) as f32],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }

    let mut indices = Vec::new();
    for iz in 0..(n - 1) {
        for ix in 0..(n - 1) {
            let i = (iz * n + ix) as u32;
            // Two triangles per cell, counter-clockwise winding.
            indices.push(i);
            indices.push(i + n as u32);
            indices.push(i + 1);
            indices.push(i + 1);
            indices.push(i + n as u32);
            indices.push(i + n as u32 + 1);
        }
    }

    (vertices, indices)
}

/// Builds a flat disc mesh (triangle fan) with `segments` sides, radius `radius`,
/// centred at the origin in the XZ plane (y = 0). Normals point up (+Y).
pub fn build_disc_mesh(radius: f32, segments: u32) -> (Vec<regia_render::LabVertex>, Vec<u32>) {
    let segs = segments.max(3);
    let mut vertices = Vec::with_capacity(segs as usize + 1);

    let vertex = |position: [f32; 3], uv: [f32; 2]| regia_render::LabVertex {
        position,
        normal: [0.0, 1.0, 0.0],
        uv,
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    };

    // Centre vertex.
    vertices.push(vertex([0.0, 0.0, 0.0], [0.5, 0.5]));

    for i in 0..segs {
        let angle = i as f32 * std::f32::consts::TAU / segs as f32;
        let x = radius * angle.cos();
        let z = radius * angle.sin();
        vertices.push(vertex(
            [x, 0.0, z],
            [0.5 + 0.5 * angle.cos(), 0.5 + 0.5 * angle.sin()],
        ));
    }

    let mut indices = Vec::new();
    for i in 0..segs {
        indices.push(0);
        indices.push(i + 1);
        indices.push(if i + 1 == segs { 1 } else { i + 2 });
    }

    (vertices, indices)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec2;
    use regia_framework::settings::SettingsPath;
    use regia_framework::sim_bridge::SimId;
    use regia_render::{ExtractedFrame, FrameRenderer, RenderDevice};

    /// Creates a headless app with the sample plugins and manual time control.
    fn make_app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.insert_resource(SettingsPath(None));
        app.add_plugins((
            MinimalPlugins,
            // Headless: extraction runs (the ECS render path under test), but no
            // window surfaces are created in a test process.
            regia_render::RenderPlugin { headless: true },
            SimBridgeSample,
        ));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app
    }

    #[test]
    fn three_sim_entities_after_5_updates() {
        let mut app = make_app();
        for _ in 0..5 {
            app.update();
        }

        let world = app.world_mut();
        let mut query = world.query::<&SimId>();
        let ids: Vec<u64> = query.iter(world).map(|sid| sid.0).collect();

        assert_eq!(
            ids.len(),
            3,
            "expected 3 entities with SimId after 5 updates, got {ids:?}"
        );
    }

    #[test]
    fn move_input_is_written_to_bridge_input() {
        let mut app = make_app();
        app.update(); // let plugins initialise

        // The reconciliation system reads BridgeInput and calls submit_input.
        // The HoverSim stores it in last_input.
        let sim = app.world().resource::<HoverSim>();
        // After 2 updates, the sim should have received at least one input.
        // We don't press any keys in this test, so move_axes should be zero.
        assert!(
            sim.last_input.is_some(),
            "submit_input should have been called at least once"
        );
        let last = sim.last_input.as_ref().expect("checked above");
        assert_eq!(last.move_axes, Vec2::ZERO, "no keys pressed, so axes are zero");
    }

    #[test]
    fn first_sim_event_triggers_audio_play() {
        let mut app = make_app();
        app.update();
        app.update(); // events drain on first frame; audio plays on second or later

        let audio = app.world().resource::<Audio>();
        assert!(
            audio.played_count() >= 1,
            "expected at least one one-shot played, got {}",
            audio.played_count()
        );
        let played = audio.played();
        assert!(!played.is_empty(), "played list should not be empty");
        assert_eq!(played[0].clip_len, 16);
        assert_eq!(played[0].sample_rate, 48_000);
    }

    /// The ECS render path renders through `MeshRenderer` + `Camera` + lights:
    /// extraction must collect the heightfield + three discs and a camera view.
    #[test]
    fn extracted_frame_has_four_meshes_and_a_camera() {
        let mut app = make_app();
        // Run enough frames for the sim to tick and the camera to settle.
        for _ in 0..10 {
            app.update();
        }

        let frame = app.world().resource::<ExtractedFrame>();
        assert_eq!(
            frame.meshes.len(),
            4,
            "expected 1 heightfield + 3 discs = 4 meshes, got {}",
            frame.meshes.len()
        );
        assert_eq!(frame.window_views().len(), 1, "the chase camera is a view");
        let directional = frame
            .directional
            .as_ref()
            .unwrap_or_else(|| panic!("the sun light is extracted"));
        assert_eq!(directional.illuminance, 100_000.0);
    }

    /// Rendering the extracted frame through the persistent renderer draws lit
    /// pixels (the runtime path end to end, on a real GPU).
    #[test]
    fn extracted_frame_renders_lit_pixels() {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
        };
        let mut app = make_app();
        for _ in 0..10 {
            app.update();
        }
        let frame = app.world().resource::<ExtractedFrame>().clone();
        let scene = frame.frame_scene(0, 64, 64);
        let mut renderer = FrameRenderer::new();
        let result = renderer
            .render_offscreen(&device, &scene)
            .expect("runtime frame render");
        assert_eq!(result.pixels.len(), (64 * 64 * 4) as usize);
        let max_c = result
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| px[0].max(px[1]).max(px[2]))
            .max()
            .unwrap_or(0);
        // Ambient-only (the pre-fix bug) topped out around channel 15. Direct sun must exceed that.
        assert!(
            max_c > 80,
            "expected direct lighting on heightfield+disc (max channel > 80), got {max_c}"
        );
    }

    #[test]
    fn m9_presentation_scripts_and_demo_systems() {
        use regia_framework::script::Scripts;

        let mut app = make_app();
        for _ in 0..30 {
            app.update();
        }
        let world = app.world_mut();
        let mut scripts = world.query_filtered::<&Scripts, ()>();
        let with_scripts = scripts.iter(world).count();
        assert!(with_scripts >= 3, "each disc should carry a presentation script");

        let log = world.resource::<PresenterLog>();
        assert!(
            log.0.iter().any(|l| l.starts_with("presenter:lean=")),
            "presenter should read sim extras: {:?}",
            log.0
        );

        let demo = world.resource::<M9DemoState>();
        assert!(demo.timer_fired, "timer should bump settings");
        assert!(demo.settings_live, "settings plugin should apply live");
        assert!(
            demo.toast_seen,
            "event router should raise a toast on sim_started"
        );
    }

    #[test]
    fn m9_camera_modes_switch() {
        let mut app = make_app();
        app.update();
        let rig_entity = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<Entity, With<CameraRig>>();
            q.iter(world).next().expect("camera rig")
        };
        {
            let mut rig = app.world_mut().get_mut::<CameraRig>(rig_entity).expect("rig");
            rig.switch_to(CameraMode::Overhead(Overhead::default()));
        }
        app.update();
        assert_eq!(
            app.world()
                .get::<CameraRig>(rig_entity)
                .expect("rig after overhead switch")
                .mode
                .name(),
            "overhead"
        );
        app.world_mut()
            .get_mut::<CameraRig>(rig_entity)
            .expect("rig for free switch")
            .switch_to(CameraMode::Free(Default::default()));
        app.update();
        assert_eq!(
            app.world()
                .get::<CameraRig>(rig_entity)
                .expect("rig after free switch")
                .mode
                .name(),
            "free"
        );
    }

    #[test]
    fn m9_target_tween_advances() {
        let mut app = make_app();
        for _ in 0..220 {
            app.update();
        }
        let world = app.world_mut();
        let chase = world
            .query::<&ChaseCamera>()
            .iter(world)
            .next()
            .expect("chase camera");
        let y = world
            .get::<Transform>(chase.target)
            .expect("look-at target")
            .translation
            .y;
        assert!(
            (y - 2.25).abs() < 0.05,
            "target should finish near 2.25 after tween, got {y}"
        );
    }

    /// Spawns a primary window of 1920×1080 physical pixels and returns its entity.
    fn spawn_touch_window(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((
                regia_platform::Window {
                    physical_size: (1920, 1080),
                    ..Default::default()
                },
                regia_platform::PrimaryWindow,
            ))
            .id()
    }

    fn touch(
        id: u64,
        phase: regia_platform::TouchPhase,
        x: f64,
        y: f64,
        window: Entity,
    ) -> regia_platform::TouchInput {
        regia_platform::TouchInput {
            window,
            id,
            phase,
            position: (x, y),
            timestamp: regia_platform::InputTimestamp::ZERO,
        }
    }

    #[test]
    fn touch_left_half_drag_drives_move_input() {
        let mut app = make_app();
        let window = spawn_touch_window(&mut app);
        app.update(); // let plugins initialise

        // Press on the left half and drag up: full forward on the virtual stick.
        let world = app.world_mut();
        world.write_message(touch(
            1,
            regia_platform::TouchPhase::Started,
            200.0,
            900.0,
            window,
        ));
        world.write_message(touch(1, regia_platform::TouchPhase::Moved, 200.0, 600.0, window));
        app.update();

        let controls = app.world().resource::<TouchControls>();
        assert!(
            controls.move_axes().y > 0.9,
            "drag up = forward, got {}",
            controls.move_axes()
        );
        assert_eq!(controls.move_axes().x, 0.0);

        // The bridge received the combined value the same frame.
        assert!(
            app.world().resource::<BridgeInput>().move_axes.y > 0.9,
            "BridgeInput should carry the touch move value"
        );

        // Lifting the finger stops the movement.
        app.world_mut()
            .write_message(touch(1, regia_platform::TouchPhase::Ended, 200.0, 600.0, window));
        app.update();
        assert_eq!(app.world().resource::<BridgeInput>().move_axes, Vec2::ZERO);
    }

    #[test]
    fn touch_right_half_drag_turns_the_camera() {
        let mut app = make_app();
        let window = spawn_touch_window(&mut app);
        app.update();

        let before = app
            .world_mut()
            .query::<&CameraRig>()
            .single(app.world())
            .expect("one chase camera")
            .yaw;
        let world = app.world_mut();
        world.write_message(touch(
            4,
            regia_platform::TouchPhase::Started,
            1500.0,
            400.0,
            window,
        ));
        world.write_message(touch(4, regia_platform::TouchPhase::Moved, 1580.0, 400.0, window));
        app.update();

        let after = app
            .world_mut()
            .query::<&CameraRig>()
            .single(app.world())
            .expect("one chase camera")
            .yaw;
        let expected_turn = 80.0 * touch::LOOK_RADIANS_PER_PIXEL;
        assert!(
            (before - after - expected_turn).abs() < 1e-4,
            "dragging right turns the camera right: {before} -> {after}"
        );
    }

    #[test]
    fn about_asset_resolves_through_the_vfs() {
        let mut app = App::new();
        app.add_plugins((regia_platform::PlatformPlugin, SimBridgeSample));
        app.update();

        let vfs = app.world().resource::<Vfs>();
        let path = VfsPath::parse("assets://about.txt").expect("valid path");
        let bytes = vfs.read_sync(&path).expect("assets://about.txt must resolve");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.starts_with("Sim Bridge"),
            "unexpected about.txt content: {text}"
        );
    }
}
