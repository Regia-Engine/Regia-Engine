// SPDX-License-Identifier: MIT-0
//! `render_lab --window sponza`: the ECS runtime path in a real window
//! (`specs/13` § 13, M5 round 5).
//!
//! Sponza is loaded through the same glTF cook path as the goldens and spawned
//! as ECS entities ([`regia_render::spawn_lab_scene`]); the frame is then
//! rendered by the renderer's runtime path — extraction, prepass, clusters,
//! shadows, forward, IBL and post — into the window surface, and serves
//! `seeing.screenshot` captures like any Regia Engine game.
//!
//! Controls: drag with the left mouse button to orbit, wheel to zoom,
//! W/S/A/D to pan, Q/E to move down/up.

use std::collections::HashSet;

use glam::{Quat, Vec2, Vec3};
use regia::prelude::*;
use regia_render::{Camera, spawn_lab_scene};
use regia_scene::Transform;
use regia_test_utils::scene_loader::{LabView, ensure_model, load_gltf_model_with_view};

/// Which model the window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowModel {
    /// The Khronos Sponza atrium (nave view).
    Sponza,
}

impl WindowModel {
    /// The glTF sample-models name.
    fn model_name(self) -> &'static str {
        match self {
            Self::Sponza => "Sponza",
        }
    }
}

/// The windowed lab plugin: loads the model and drives the camera.
pub struct WindowedLab {
    /// Model to load and render.
    pub model: WindowModel,
    /// Exit after this many frames (0 = run until closed).
    pub exit_after_frames: u32,
    /// When set, capture a screenshot to this path (taken a few frames before
    /// `exit_after_frames` so the request is always served) and exit.
    pub screenshot: Option<std::path::PathBuf>,
}

/// Orbit + fly camera state (the pivot sits at the look-at point).
#[derive(Resource)]
struct OrbitState {
    target: Vec3,
    distance: f32,
    yaw: f32,
    pitch: f32,
    dragging: bool,
    held: HashSet<String>,
}

/// Screenshot/exit plan for automated runs.
#[derive(Resource, Clone, Default)]
struct CapturePlan {
    /// Exit after this many frames (0 = run until closed).
    exit_after_frames: u32,
    /// When set, capture a screenshot to this path a few frames before the
    /// exit frame and wait for it to be written before exiting.
    screenshot: Option<std::path::PathBuf>,
}

/// Set when the screenshot PNG has been written.
#[derive(Resource, Default)]
struct ScreenshotTaken(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Plugin for WindowedLab {
    fn build(&self, app: &mut App) {
        let model = self.model;
        app.insert_resource(CapturePlan {
            exit_after_frames: self.exit_after_frames,
            screenshot: self.screenshot.clone(),
        })
        .init_resource::<ScreenshotTaken>()
        .add_systems(Startup, move |world: &mut World| setup(world, model))
        .add_systems(Update, orbit_control)
        .add_systems(Last, frame_tick);
    }
}

/// `Startup`: load the glTF model and spawn the ECS scene.
fn setup(world: &mut World, model: WindowModel) {
    let path = match ensure_model(model.model_name()) {
        Ok(path) => path,
        Err(err) => {
            tracing::error!("render_lab: {err}");
            return;
        }
    };
    let lab = match load_gltf_model_with_view(&path, LabView::SponzaAtrium, None) {
        Ok(lab) => lab,
        Err(err) => {
            tracing::error!("render_lab: glTF load failed: {err}");
            return;
        }
    };
    let eye = Vec3::from_array(lab.eye);
    let target = Vec3::from_array(lab.target);
    tracing::info!(
        "render_lab window: {} meshes, {} materials; camera {:?} → {:?}",
        lab.meshes.len(),
        lab.materials.len(),
        lab.eye,
        lab.target
    );
    spawn_lab_scene(world, &lab);
    // Orbit around the look-at point at the golden view's distance.
    let offset = eye - target;
    let distance = offset.length().max(0.5);
    let yaw = offset.z.atan2(offset.x);
    let pitch = offset.y.atan2(Vec2::new(offset.x, offset.z).length());
    world.insert_resource(OrbitState {
        target,
        distance,
        yaw,
        pitch: pitch.clamp(-1.45, 1.45),
        dragging: false,
        held: HashSet::new(),
    });
}

/// `Update`: orbit (drag) + fly (WASD/QE) camera control.
fn orbit_control(
    motion: Res<Messages<MouseMotion>>,
    buttons: Res<Messages<MouseButtonInput>>,
    wheel: Res<Messages<MouseWheel>>,
    keys: Res<Messages<KeyboardInput>>,
    mut state: ResMut<OrbitState>,
    mut cameras: Query<&mut Transform, With<Camera>>,
) {
    for button in buttons.iter_current_update_messages() {
        match (button.button, button.state) {
            (MouseButton::Left, ButtonState::Pressed) => state.dragging = true,
            (MouseButton::Left, ButtonState::Released) => state.dragging = false,
            _ => {}
        }
    }
    for key in keys.iter_current_update_messages() {
        if key.state == ButtonState::Pressed {
            state.held.insert(key.physical_key.clone());
        } else {
            state.held.remove(&key.physical_key);
        }
    }
    let mut delta = Vec2::ZERO;
    if state.dragging {
        for event in motion.iter_current_update_messages() {
            delta += Vec2::new(event.delta.0 as f32, event.delta.1 as f32);
        }
    }
    state.yaw -= delta.x * 0.005;
    state.pitch = (state.pitch - delta.y * 0.005).clamp(-1.45, 1.45);
    for event in wheel.iter_current_update_messages() {
        let amount = if event.unit == regia::platform::WheelUnit::Pixel {
            event.delta.1 * 0.002
        } else {
            event.delta.1 * 0.4
        };
        state.distance = (state.distance - amount).clamp(0.8, 60.0);
    }

    // Fly/pan: move the orbit pivot along the camera's axes.
    let forward = Vec3::new(
        state.yaw.cos() * state.pitch.cos(),
        state.pitch.sin(),
        state.yaw.sin() * state.pitch.cos(),
    );
    let right = Vec3::new(-state.yaw.sin(), 0.0, state.yaw.cos());
    let speed = state.distance * 0.04;
    let mut pan = Vec3::ZERO;
    if state.held.contains("KeyW") {
        pan += forward * speed;
    }
    if state.held.contains("KeyS") {
        pan -= forward * speed;
    }
    if state.held.contains("KeyD") {
        pan += right * speed;
    }
    if state.held.contains("KeyA") {
        pan -= right * speed;
    }
    if state.held.contains("KeyE") {
        pan.y += speed;
    }
    if state.held.contains("KeyQ") {
        pan.y -= speed;
    }
    state.target += pan;

    // Write the camera transform.
    let eye = state.target
        + Vec3::new(
            state.yaw.cos() * state.pitch.cos(),
            state.pitch.sin(),
            state.yaw.sin() * state.pitch.cos(),
        ) * state.distance;
    let look = (state.target - eye).normalize_or_zero();
    if look.length_squared() < 1.0e-6 {
        return;
    }
    let rotation = Quat::from_rotation_arc(-Vec3::Z, look);
    for mut transform in &mut cameras {
        transform.translation = eye;
        transform.rotation = rotation;
    }
}

/// `Last`: frame counting, screenshot capture and exit.
fn frame_tick(
    time: Res<Time>,
    plan: Res<CapturePlan>,
    taken: Res<ScreenshotTaken>,
    mut requests: ResMut<regia_render::ScreenshotRequests>,
    mut exit: MessageWriter<AppExit>,
) {
    let frames = time.frame_count();
    // Request the screenshot a few frames before the exit frame so the
    // renderer has time to serve it.
    if let Some(path) = &plan.screenshot
        && plan.exit_after_frames > 4
        && frames == u64::from(plan.exit_after_frames) - 4
    {
        let path = path.clone();
        let written = taken.0.clone();
        requests.request(
            None,
            Box::new(move |result| match result {
                Ok(image) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match image::save_buffer(
                        &path,
                        &image.rgba,
                        image.width,
                        image.height,
                        image::ColorType::Rgba8,
                    ) {
                        Ok(()) => tracing::info!("render_lab: screenshot saved to {}", path.display()),
                        Err(err) => tracing::error!("render_lab: screenshot save failed: {err}"),
                    }
                    written.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Err(err) => tracing::error!("render_lab: screenshot failed: {err}"),
            }),
        );
    }

    if plan.exit_after_frames > 0 && frames + 1 >= u64::from(plan.exit_after_frames) {
        // Only exit once the requested screenshot has been written.
        let written = taken.0.load(std::sync::atomic::Ordering::SeqCst);
        if plan.screenshot.as_ref().is_none_or(|_| written) {
            tracing::info!("render_lab: exit after {} frames", plan.exit_after_frames);
            exit.write(AppExit::Success);
        }
    }
}
