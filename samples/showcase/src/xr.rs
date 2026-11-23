// SPDX-License-Identifier: MIT-0
//! Walking the showcase scene in a headset (`--xr`, ADR-2457, book `agents/xr-walk.md`).
//!
//! [`ShowcaseXrPlugin`] adds the engine's XR plugins to the showcase app and wires them to the scene:
//!
//! - `regia_xr`'s [`XrPlugin`] (the session; the simulator or a real runtime) and, with a renderer,
//!   [`XrRenderPlugin`] (both eyes, the XR quality profile, the comfort vignette, the desktop mirror);
//! - [`XrWalkPlugin`] with the terrain as its [`XrGround`], so the floor follows the heightfield under the
//!   head;
//! - a rig at the scene camera's ground point, facing where the camera looks, with the head's [`XrEars`] as the
//!   only [`AudioListener`] (binaural): spatial audio is heard from the head pose the headset reports, and a
//!   teleport is a cut for Doppler, not a pass at hundreds of meters per second;
//! - at startup the walk starts with a comfort preset and the XR budget (render scale and foveation for
//!   [`XR_REFRESH_HZ`]); on the simulator the session is taken to focused.
//!
//! The rig is not parented to the desktop camera, so nothing that moves the camera (the camera path
//! player, the shock shake) reaches the eyes: the head pose comes only from the headset. The walk adds no
//! bob and no noise. The other way round, while the walk runs the desktop camera follows the head and
//! widens to hold both eyes' fields of view ([`camera_follows_head`]): the streamed forest, mesh LODs and
//! the terrain are streamed and culled around the primary camera, so the eyes see what the head faces.

use std::sync::Arc;

use regia::audio::AudioListener;
use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_input::InputInjection;
use regia_platform::xr::{Hand, View, XrRuntime};
use regia_render::Camera;
use regia_scene::hierarchy::HierarchyCommands;
use regia_scene::{GlobalTransform, Transform};
use regia_xr::sim::ScriptState;
use regia_xr::{
    ComfortPreset, Mirror, WalkStart, XrBackendKind, XrConfig, XrController, XrGround, XrHead, XrPlugin,
    XrRenderPlugin, XrRig, XrSession, XrState, XrWalk, XrWalkPlugin,
};

use crate::scene::SceneData;

/// The display rate the walk's budget is chosen for (a 90 Hz headset), Hz.
pub const XR_REFRESH_HZ: f32 = 90.0;

/// Scatter mesh detail the walk uses unless `--scatter-lod` says otherwise: a coarser level than the
/// filming default, for the stereo budget.
pub const XR_SCATTER_LOD: usize = 2;

/// The mesh-LOD viewport of a walk (`LodViewport`): the simulator's recommended eye size (1440 x 1584)
/// at the 90 Hz budget's 0.8 render scale, so LOD switches happen at the eye's resolution, not the
/// filming size.
pub const XR_LOD_VIEWPORT: (u32, u32) = (1152, 1267);

/// The backend named on the command line (`--xr simulator | openxr | auto`).
#[must_use]
pub fn backend_from_name(name: &str) -> Option<XrBackendKind> {
    match name {
        "simulator" => Some(XrBackendKind::Simulator),
        "openxr" => Some(XrBackendKind::OpenXr),
        "auto" => Some(XrBackendKind::Auto),
        _ => None,
    }
}

/// The scene's ground for the walk: the heightfield's bilinear height on the map, `None` off it.
#[must_use]
pub fn scene_ground(scene: &Arc<SceneData>) -> XrGround {
    let data = Arc::clone(scene);
    XrGround(Arc::new(move |x: f32, z: f32| {
        let half = data.map.world_width() * 0.5;
        (x.abs() <= half && z.abs() <= half).then(|| data.map.sample_bilinear(x, z))
    }))
}

/// Walk mode for the showcase. Add after [`crate::app::ShowcasePlugin`] (and the renderer, when
/// `render` is on).
pub struct ShowcaseXrPlugin {
    /// The scene (its camera is where the walk starts; its heightfield is the ground).
    pub scene: Arc<SceneData>,
    /// Which XR backend to open.
    pub backend: XrBackendKind,
    /// Add [`XrRenderPlugin`] (needs the renderer; off for CPU-only tests).
    pub render: bool,
    /// The comfort preset the walk starts with.
    pub preset: ComfortPreset,
}

/// Where the walk starts and how (read by the startup system).
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct XrWalkPlan {
    /// The head stands over this point (the scene camera's position).
    pub position: Vec3,
    /// The heading the head faces (the scene camera's), radians.
    pub yaw: f32,
    /// The comfort preset.
    pub preset: ComfortPreset,
}

impl Plugin for ShowcaseXrPlugin {
    fn build(&self, app: &mut App) {
        let config = XrConfig {
            backend: self.backend,
            preferred_refresh_hz: Some(XR_REFRESH_HZ),
            mirror: if cfg!(target_os = "android") {
                Mirror::Off
            } else {
                Mirror::Left
            },
            ..XrConfig::default()
        };
        app.init_resource::<InputInjection>();
        app.add_plugins(XrPlugin::new(config))
            .add_plugins(XrWalkPlugin::default());
        if self.render {
            app.add_plugins(XrRenderPlugin);
        }
        let cam = self.scene.manifest.camera;
        let eye = Vec3::new(cam.position.0, cam.position.1, cam.position.2);
        let target = Vec3::new(cam.target.0, cam.target.1, cam.target.2);
        let yaw = regia_xr::space::yaw_of(crate::app::look_rotation(eye, target));
        // LOD switches at the eye's resolution, not the filming size.
        app.insert_resource(regia_render::LodViewport {
            width: XR_LOD_VIEWPORT.0,
            height: XR_LOD_VIEWPORT.1,
        });
        app.insert_resource(scene_ground(&self.scene))
            .insert_resource(XrWalkPlan {
                position: eye,
                yaw: if yaw.is_finite() { yaw } else { 0.0 },
                preset: self.preset,
            })
            .add_systems(Startup, (spawn_xr_rig, start_xr_walk).chain())
            .add_systems(
                Update,
                (fresh_ears_on_jump, ears_listen_while_running, keep_one_listener).chain(),
            )
            .add_systems(
                PostUpdate,
                camera_follows_head
                    .after(crate::shake::apply_camera_shake)
                    .before(PostUpdateSet::TransformPropagate),
            );
    }
}

/// Adds the walk to a headless app ([`crate::app::build_headless_app`]) and runs its clock, so the walk
/// moves on every update (the headless showcase otherwise starts paused for render-out jobs).
pub fn add_headless_xr(app: &mut App, scene: &Arc<SceneData>, backend: XrBackendKind, preset: ComfortPreset) {
    app.add_plugins(ShowcaseXrPlugin {
        scene: Arc::clone(scene),
        backend,
        render: true,
        preset,
    });
    if let Some(mut time) = app.world_mut().get_resource_mut::<Time>() {
        time.paused = false;
    }
}

/// Runs the windowed showcase with the walk: the engine's default plugins, the scene, XR and the desktop
/// mirror. Without a runtime (no headset, or the OpenXR binding not built) XR stays off with the reason in
/// `xr.get` and the scene runs on the desktop.
pub fn run_windowed_xr(
    scene: Arc<SceneData>,
    options: crate::app::Options,
    project_dir: Option<std::path::PathBuf>,
    window: crate::app::WindowOptions,
    replay_roots: Vec<std::path::PathBuf>,
    backend: XrBackendKind,
    preset: ComfortPreset,
) -> crate::app::AppExit {
    let xr = ShowcaseXrPlugin {
        scene: Arc::clone(&scene),
        backend,
        render: true,
        preset,
    };
    regia::run_with((crate::app::ShowcasePlugin { scene, options }, xr), move |app| {
        crate::app::configure_windowed(app, project_dir, window, replay_roots);
    })
}

/// `Startup`: the rig, the head (the audio listener) and both controllers.
fn spawn_xr_rig(mut commands: Commands) {
    let rig = commands
        .spawn((XrRig, Transform::IDENTITY, GlobalTransform::default()))
        .id();
    let head = commands
        .spawn((XrHead, Transform::IDENTITY, GlobalTransform::default()))
        .set_parent(rig)
        .id();
    spawn_ears(&mut commands, head);
    for hand in [Hand::Left, Hand::Right] {
        commands
            .spawn((
                XrController { hand },
                Transform::IDENTITY,
                GlobalTransform::default(),
            ))
            .set_parent(rig);
    }
}

/// `Startup`, after the rig: focus the simulator, then start the walk at the scene camera.
fn start_xr_walk(world: &mut World) {
    let Some(plan) = world.get_resource::<XrWalkPlan>().copied() else {
        return;
    };
    let simulator = world
        .get_resource::<XrRuntime>()
        .is_some_and(|rt| rt.backend().name() == "null");
    if simulator && let Err(e) = regia_xr::drive_session_state(world, ScriptState::Focused) {
        tracing::warn!(target: "showcase", error = %e, "xr: the simulator did not reach focused");
    }
    let start = WalkStart {
        position: Some(plan.position),
        yaw: Some(plan.yaw),
        preset: Some(plan.preset),
        budget: true,
    };
    match regia_xr::walk_start(world, start) {
        Ok(()) => tracing::info!(
            target: "showcase",
            preset = plan.preset.name(),
            "xr: walk started at the scene camera"
        ),
        Err(e) => tracing::warn!(target: "showcase", error = %e, "xr: the walk did not start"),
    }
}

/// The audio listener of the walk: a child of the head at its pose (binaural). It is replaced by a new
/// entity on every jump of the rig ([`XrWalk::jumps`]): the mixer derives the listener's Doppler velocity
/// from its motion per entity, and a teleport of a few meters in one frame would otherwise sound like a
/// pass at hundreds of meters per second. A new entity starts at rest.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XrEars;

fn spawn_ears(commands: &mut Commands, head: Entity) {
    commands
        .spawn((XrEars, Transform::IDENTITY, GlobalTransform::default()))
        .set_parent(head);
}

/// `Update`: the ears listen (binaural) only while a session runs; without one (no headset, no runtime) the
/// desktop keeps its own listener.
fn ears_listen_while_running(
    mut commands: Commands,
    state: Option<Res<XrState>>,
    ears: Query<(Entity, Option<&AudioListener>), With<XrEars>>,
) {
    let running = state.is_some_and(|s| s.app.is_running());
    for (entity, listener) in &ears {
        let listening = listener.is_some();
        if running && !listening {
            commands.entity(entity).insert(AudioListener { hrtf: true });
        } else if !running && listening {
            commands.entity(entity).remove::<AudioListener>();
        }
    }
}

/// `Update`: after a jump of the rig, new ears (see [`XrEars`]).
fn fresh_ears_on_jump(
    mut commands: Commands,
    walk: Option<Res<XrWalk>>,
    mut seen: Local<u64>,
    ears: Query<Entity, With<XrEars>>,
    heads: Query<Entity, With<XrHead>>,
) {
    let Some(jumps) = walk.map(|w| w.jumps()) else {
        return;
    };
    if jumps == *seen {
        return;
    }
    *seen = jumps;
    let Some(head) = heads.iter().next() else {
        return;
    };
    for entity in &ears {
        commands.entity(entity).despawn();
    }
    spawn_ears(&mut commands, head);
}

/// `Update`: in a headset the ears are the only listener, so no other listener (a camera's) splits
/// the scene's emitters with them.
fn keep_one_listener(
    mut commands: Commands,
    others: Query<Entity, (With<AudioListener>, Without<XrEars>)>,
    ears: Query<(), (With<AudioListener>, With<XrEars>)>,
) {
    if ears.is_empty() {
        return;
    }
    for entity in &others {
        commands.entity(entity).remove::<AudioListener>();
    }
}

/// Margin on the covering field of view (5 %).
pub const COVER_MARGIN: f32 = 1.05;

/// Widest covering vertical field of view, radians (170 degrees).
pub const MAX_COVER_FOV_Y: f32 = 2.967;

/// The vertical field of view (radians) of a head-aligned camera at `aspect` (width / height) whose frustum
/// holds both eyes' fields of view: each eye's edges are taken relative to the head (`head` orientation, so a
/// canted display widens it), then the widest tangent, with [`COVER_MARGIN`], capped at [`MAX_COVER_FOV_Y`].
#[must_use]
pub fn covering_fov_y(views: &[View; 2], head: Quat, aspect: f32) -> f32 {
    let limit = 89f32.to_radians();
    let (mut th, mut tv) = (0f32, 0f32);
    for view in views {
        let cant = regia_xr::space::yaw_of(head.inverse() * view.pose.orientation);
        let cant = if cant.is_finite() { cant } else { 0.0 };
        for edge in [view.fov.left - cant, view.fov.right - cant] {
            th = th.max(edge.abs().min(limit).tan());
        }
        for edge in [view.fov.up, view.fov.down] {
            tv = tv.max(edge.abs().min(limit).tan());
        }
    }
    let t = tv.max(th / aspect.max(1e-3)) * COVER_MARGIN;
    (2.0 * t.atan()).min(MAX_COVER_FOV_Y)
}

/// The rig, not the head or a camera.
type RigOnly = (With<XrRig>, Without<Camera>, Without<XrHead>);
/// The head, not the rig or a camera.
type HeadOnly = (With<XrHead>, Without<Camera>, Without<XrRig>);
/// A camera, not the rig or the head.
type CameraOnly = (With<Camera>, Without<XrRig>, Without<XrHead>);

/// `PostUpdate`, after the path player and the shake: while the walk runs and frames flow, the desktop
/// camera takes the head's world pose and a field of view that holds both eyes ([`covering_fov_y`] at the
/// `LodViewport`'s aspect), so everything streamed or culled around the primary camera follows the head.
pub fn camera_follows_head(
    walk: Option<Res<XrWalk>>,
    state: Option<Res<XrState>>,
    xr: (Option<Res<XrRuntime>>, Option<Res<XrSession>>),
    viewport: Option<Res<regia_render::LodViewport>>,
    rigs: Query<&Transform, RigOnly>,
    heads: Query<&Transform, HeadOnly>,
    mut cameras: Query<(&mut Transform, &mut Camera), CameraOnly>,
) {
    let (runtime, session) = xr;
    let _span = tracing::trace_span!("showcase_xr_camera_follow").entered();
    let walking = walk.is_some_and(|w| w.active) && state.is_some_and(|s| s.app.is_running());
    if !walking {
        return;
    }
    let (Some(rig), Some(head)) = (rigs.iter().next(), heads.iter().next()) else {
        return;
    };
    let (position, rotation) = head_world(rig, head);
    let aspect = viewport.map_or(1.0, |v| v.width.max(1) as f32 / v.height.max(1) as f32);
    // The views are in tracking space: the cant is measured against the tracked head, not the rig-local one.
    let tracked = session.map_or(Quat::IDENTITY, |s| s.held_head().orientation);
    let fov_y = runtime
        .and_then(|rt| rt.last_report().map(|r| r.views))
        .map(|views| covering_fov_y(&views, tracked, aspect));
    for (mut transform, mut camera) in &mut cameras {
        transform.translation = position;
        transform.rotation = rotation;
        if let Some(fov) = fov_y
            && (camera.fov_y - fov).abs() > 1e-6
        {
            camera.fov_y = fov;
        }
    }
}

/// The head's world pose: the rig's transform composed with the head's (rig-local) transform.
#[must_use]
pub fn head_world(rig: &Transform, head: &Transform) -> (Vec3, Quat) {
    (
        rig.translation + rig.rotation * head.translation,
        rig.rotation * head.rotation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_parse() {
        assert_eq!(backend_from_name("simulator"), Some(XrBackendKind::Simulator));
        assert_eq!(backend_from_name("openxr"), Some(XrBackendKind::OpenXr));
        assert_eq!(backend_from_name("auto"), Some(XrBackendKind::Auto));
        assert_eq!(backend_from_name("vr"), None);
    }

    #[test]
    fn the_covering_field_of_view_holds_both_eyes() {
        use regia_platform::xr::{Eye, Fov, Pose};
        let half = 50f32.to_radians();
        let eye = |e, yaw: f32| View {
            eye: e,
            pose: Pose::new(Vec3::ZERO, Quat::from_rotation_y(yaw)),
            fov: Fov {
                left: -half,
                right: half,
                up: half,
                down: -half,
            },
        };
        let views = [eye(Eye::Left, 0.0), eye(Eye::Right, 0.0)];
        let fov = covering_fov_y(&views, Quat::IDENTITY, 1.0);
        assert!(
            (fov - 2.0 * (half.tan() * COVER_MARGIN).atan()).abs() < 1e-5,
            "{fov}"
        );
        // A narrow viewport needs a taller field of view to hold the same width.
        assert!(covering_fov_y(&views, Quat::IDENTITY, 0.5) > fov);
        // Canted displays (each eye turned 10 degrees outward) widen it.
        let canted = [
            eye(Eye::Left, 10f32.to_radians()),
            eye(Eye::Right, -10f32.to_radians()),
        ];
        assert!(covering_fov_y(&canted, Quat::IDENTITY, 1.0) > fov);
        // The head's own turn is not a cant.
        let turned = [eye(Eye::Left, 1.0), eye(Eye::Right, 1.0)];
        assert!((covering_fov_y(&turned, Quat::from_rotation_y(1.0), 1.0) - fov).abs() < 1e-4);
        assert!(covering_fov_y(&views, Quat::IDENTITY, 0.01) <= MAX_COVER_FOV_Y);
    }

    #[test]
    fn the_lod_viewport_is_the_budgeted_eye() {
        let budget = regia_xr::walk_budget(XR_REFRESH_HZ);
        assert_eq!(XR_LOD_VIEWPORT.0, (1440.0 * budget.render_scale).round() as u32);
        assert_eq!(XR_LOD_VIEWPORT.1, (1584.0 * budget.render_scale).round() as u32);
    }
}
