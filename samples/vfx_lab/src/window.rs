// SPDX-License-Identifier: MIT-0
//! `vfx_lab --window`: the ECS runtime path in a real window (M14 round 2).
//!
//! A lit ground + boxes + sky rendered through `regia_render`'s runtime path
//! (Camera / MeshRenderer / DirectionalLight / Environment entities) with the full
//! DO27 effect set cycling on top: every ~2.5 s the next effect fires (muzzle,
//! tracer, impact, explosion, exhaust, dust, sparks, weather, smoke), explosions
//! scorched the ground with decals and shake the camera, a trail rig flies a
//! figure-eight. VFX renders through the `TranslucentLayer` hook
//! ([`regia_vfx::VfxRuntimePlugin`]); the window serves agent screenshots.
//!
//! Round-3 root cause fixed here: the app must add `regia_scene::ScenePlugin` —
//! `Transform`'s required components only insert an *identity* `GlobalTransform`;
//! without the plugin's propagation system the runtime renderer extracted a camera
//! at the origin looking −Z and every mesh at an identity model matrix (round 2's
//! "near-black frame, huge particles, no scene"). The sky is an emissive dome in
//! physical nits (`SKY_IBL_SCALE`), so it survives the EV100 exposure pass; the
//! frame is ground + sky + effects.
//!
//! Controls: drag with the left mouse button to orbit, wheel to zoom, WASD to pan,
//! Q/E down/up.

use std::collections::HashSet;
use std::sync::Arc;

use glam::{Quat, Vec2, Vec3};
use regia::prelude::*;
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabTexture, MeshRenderer, PostSettings,
    RenderSettings, ScreenshotRequests,
};
use regia_scene::{GlobalTransform, Transform};
use regia_vfx::{CameraShakeRequest, SpawnDecal, SpawnEffect, VfxPlugin, VfxRuntimePlugin, VfxTrails};

use regia_core::math::Vec3 as FVec3;

/// The windowed lab plugin.
pub struct WindowedVfxLab {
    /// Exit after this many frames (0 = run until closed).
    pub exit_after_frames: u32,
    /// When set, capture a screenshot to this path a few frames before exiting.
    pub screenshot: Option<std::path::PathBuf>,
}

/// Orbit + fly camera state.
#[derive(Resource)]
pub(crate) struct OrbitState {
    target: Vec3,
    /// Eye distance from the target.
    pub(crate) distance: f32,
    /// Heading, radians.
    pub(crate) yaw: f32,
    /// Elevation, radians.
    pub(crate) pitch: f32,
    dragging: bool,
    held: HashSet<String>,
}

/// Screenshot/exit plan.
#[derive(Resource, Clone, Default)]
struct CapturePlan {
    exit_after_frames: u32,
    screenshot: Option<std::path::PathBuf>,
}

#[derive(Resource, Default)]
struct ScreenshotTaken(Arc<std::sync::atomic::AtomicBool>);

/// The effect director state.
#[derive(Resource)]
struct Director {
    /// Last showcase slot already fired (`u64::MAX` before the first).
    last_slot: u64,
    /// Index into the showcase set of the effect that is live.
    index: usize,
    /// Total elapsed seconds (the trail rig integrates this).
    time: f32,
    /// A trail rig following a figure-eight.
    trail: Option<regia_vfx::TrailRig>,
}

impl Plugin for WindowedVfxLab {
    fn build(&self, app: &mut App) {
        app.insert_resource(CapturePlan {
            exit_after_frames: self.exit_after_frames,
            screenshot: self.screenshot.clone(),
        })
        .init_resource::<ScreenshotTaken>()
        // The scene plugin owns Transform → GlobalTransform propagation (PostUpdate,
        // before Render extraction). Without it every GlobalTransform stays the
        // auto-inserted identity: the extracted camera sits at the origin inside the
        // effects, the ground renders edge-on and the boxes collapse onto the camera
        // (the round-2 window defect — see tests/runtime_window.rs).
        .add_plugins(regia_scene::ScenePlugin)
        .add_plugins((VfxPlugin, VfxRuntimePlugin))
        // Bloom off: the scene's dynamic range is display-level after EV15, and the
        // effect pass draws after the post stack anyway (it would never see bloom).
        .insert_resource(RenderSettings {
            post: PostSettings {
                bloom: false,
                ..PostSettings::default()
            },
            ..RenderSettings::default()
        })
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (orbit_control, effect_director, trail_director, apply_shake).chain(),
        )
        .add_systems(Last, frame_tick);
    }
}

/// Startup: ground + reference boxes, sun, camera, VFX library.
fn setup(world: &mut World) {
    // The plugin's Startup ordering is not guaranteed relative to ours; make sure the
    // scene exists before loading the showcase set into it (idempotent either way).
    if world.get_resource::<regia_vfx::VfxScene>().is_none() {
        world.insert_resource(regia_vfx::VfxScene::new(regia_vfx::scene::VfxLibrary::new()));
    }
    for def in regia_vfx::showcase_effects() {
        let name = def.name.clone();
        let mut scene = world.resource_mut::<regia_vfx::VfxScene>();
        if let Err(err) = scene.library_mut().add(def) {
            tracing::error!(target: "vfx_lab", "showcase {name}: {err}");
        }
    }

    // Ground: a 200 m grid plane.
    let ground = Arc::new(CpuMesh {
        vertices: ground_vertices(200.0, 100),
        indices: ground_indices(100),
    });
    let ground_mat = Arc::new(CpuMaterial {
        base_color: [0.13, 0.25, 0.08, 1.0],
        metallic: 0.0,
        roughness: 0.95,
        ..Default::default()
    });
    // `GlobalTransform` is pre-filled (like `spawn_lab_scene`) so the very first frame
    // renders correctly even before the propagation system's first PostUpdate run.
    world.spawn((
        regia_scene::components::Name::new("ground"),
        Transform::default(),
        GlobalTransform::default(),
        MeshRenderer {
            mesh: ground,
            material: ground_mat,
            layer: 0,
            decal_layer: 0,
            gi_proxy: true,
        },
    ));

    // Sky dome: a hemisphere of inward-facing triangles around the camera. The colour
    // is emissive in physical nits (`SKY_IBL_SCALE` convention) so it survives the
    // EV100 exposure pass. Albedo stays black and `unlit` stays off: the unlit path
    // returns base colour and drops emissive, which would paint this dome black.
    let sky = Arc::new(CpuMesh {
        vertices: sky_dome_vertices(300.0, 24, 12),
        indices: sky_dome_indices(24, 12),
    });
    let sky_mat = Arc::new(sky_material());
    world.spawn((
        regia_scene::components::Name::new("sky"),
        Transform::default(),
        GlobalTransform::default(),
        MeshRenderer {
            mesh: sky,
            material: sky_mat,
            layer: 0,
            decal_layer: 0,
            gi_proxy: true,
        },
    ));

    // Reference boxes for depth context.
    let box_mesh = Arc::new(CpuMesh {
        vertices: box_vertices(1.0, 0.6, 1.0),
        indices: box_indices(),
    });
    for (i, position) in [
        Vec3::new(-6.0, 0.3, -3.0),
        Vec3::new(-4.2, 0.3, 1.5),
        Vec3::new(5.5, 0.3, -2.5),
        Vec3::new(3.2, 0.3, 4.0),
    ]
    .iter()
    .enumerate()
    {
        let tint = [
            [0.35, 0.30, 0.26, 1.0],
            [0.28, 0.33, 0.40, 1.0],
            [0.42, 0.26, 0.20, 1.0],
            [0.24, 0.36, 0.28, 1.0],
        ][i % 4];
        let transform = Transform::from_translation(*position);
        world.spawn((
            regia_scene::components::Name::new(format!("box_{i}")),
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer {
                mesh: Arc::clone(&box_mesh),
                material: Arc::new(CpuMaterial {
                    base_color: tint,
                    metallic: 0.05,
                    roughness: 0.8,
                    ..Default::default()
                }),
                layer: 0,
                decal_layer: 0,
                gi_proxy: true,
            },
        ));
    }

    // Camera: composed from the same orbit pose the control system uses, so frame 0
    // (pre-propagation) matches every later frame.
    let orbit = OrbitState {
        target: Vec3::new(0.0, 1.2, 0.0),
        distance: 11.0,
        yaw: 0.65,
        pitch: 0.22,
        dragging: false,
        held: HashSet::new(),
    };
    let (eye, rotation) = orbit_pose(&orbit);
    let camera = Transform {
        translation: eye,
        rotation,
        ..Default::default()
    };
    world.spawn((
        regia_scene::components::Name::new("camera"),
        camera,
        GlobalTransform(camera.compute_affine()),
        Camera {
            fov_y: 55.0f32.to_radians(),
            near: 0.05,
            far: 400.0,
            // EV15 matches the renderer's outdoor-sun reference (`lab_frame`'s daylight
            // setup: a 90k lux sun with EV 12.5 renders the ground ~2.5 stops too dark).
            exposure_ev: Some(15.0),
            ..Camera::default()
        },
    ));

    // Sun.
    world.spawn((
        regia_scene::components::Name::new("sun"),
        Transform::default(),
        DirectionalLight {
            direction: [-0.45, -0.62, -0.42],
            color: [1.0, 0.96, 0.88],
            illuminance: 90_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));

    // The sky's black occlusion map zeroes IBL on that mesh (roughness 1 still
    // has a split-sum bias that varies across the dome). The ground keeps IBL.
    world.insert_resource(Environment {
        ibl: true,
        ambient_lux: 12_000.0,
        double_sided: false,
    });

    world.insert_resource(orbit);
    world.insert_resource(Director {
        last_slot: u64::MAX,
        index: 0,
        time: 0.0,
        trail: Some(regia_vfx::TrailRig::new(regia_vfx::TrailStyle {
            width: 0.12,
            lifetime: 1.1,
            color: [1.0, 0.55, 0.12, 0.95],
            min_segment: 0.05,
        })),
    });
}

/// Eye position and orientation an [`OrbitState`] composes to.
fn orbit_pose(state: &OrbitState) -> (Vec3, Quat) {
    let forward = Vec3::new(
        state.yaw.cos() * state.pitch.cos(),
        state.pitch.sin(),
        state.yaw.sin() * state.pitch.cos(),
    );
    let eye = state.target + forward * state.distance;
    let look = (state.target - eye).normalize_or_zero();
    let rotation = Quat::from_rotation_arc(-Vec3::Z, look);
    (eye, rotation)
}

/// Update: orbit (drag) + fly (WASD/QE) camera control.
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
        state.distance = (state.distance - amount).clamp(2.0, 60.0);
    }

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

    let (eye, rotation) = orbit_pose(&state);
    for mut transform in &mut cameras {
        transform.translation = eye;
        transform.rotation = rotation;
    }
}

/// Update: cycles the DO27 effect set; explosions scorch + shake.
#[allow(clippy::too_many_arguments)]
fn effect_director(
    time: Res<Time>,
    mut director: ResMut<Director>,
    mut scene: ResMut<regia_vfx::VfxScene>,
    mut effects: MessageWriter<SpawnEffect>,
    mut decals: MessageWriter<SpawnDecal>,
    mut shakes: MessageWriter<CameraShakeRequest>,
    shake: Res<regia_vfx::CameraShake>,
) {
    director.time += time.delta_secs();
    // Frame slots, not wall-clock. A hidden window steps ~100 ms, so frame 236
    // (the `--exit-after-frames 240` screenshot) is ~23 s along a 2.6 s cycle
    // and landed in lingering weather mist: soft blue puffs over the whole
    // zenith, hundreds of colours on a top row. Slot 6 of this table is sparks
    // (local). Weather starts at frame 249, after that screenshot.
    const FIRST_FIRE_FRAME: u64 = 39;
    const FIRE_PERIOD: u64 = 30;
    let frame = time.frame_count();
    if frame < FIRST_FIRE_FRAME {
        return;
    }
    let slot = (frame - FIRST_FIRE_FRAME) / FIRE_PERIOD;
    if slot == director.last_slot {
        return;
    }
    director.last_slot = slot;
    let set = regia_vfx::showcase_effects();
    let names: Vec<&str> = set.iter().map(|d| d.name.as_str()).collect();
    let name = names[slot as usize % names.len()];
    director.index = slot as usize % names.len();

    // One effect at a time: emitters with `duration: None` (the tracer's firehose)
    // would otherwise run forever and pile up cycle after cycle until the scene is
    // an unwatchable soup. Stopped instances die shortly after (sim.rs).
    scene.stop_all();

    // Where this effect plays.
    let (position, forward) = match name {
        "weather" => (FVec3::new(0.0, 8.0, 0.0), FVec3::Z),
        "dust_trail" => (FVec3::new(-5.0, 0.4, 2.0), FVec3::Z),
        "smoke" => (FVec3::new(-3.5, 0.2, -3.0), FVec3::Z),
        _ => (FVec3::new(0.0, 1.2, 0.0), FVec3::Z),
    };
    effects.write(SpawnEffect {
        effect: name.into(),
        position,
        forward,
        seed: None,
    });
    tracing::info!(target: "vfx_lab", "fired {name} (slot {}) at {position:?}", director.index);

    // The blast set scorch + shake + impact marks.
    if name == "explosion" {
        decals.write(SpawnDecal {
            position: FVec3::new(0.0, 0.0, 0.0),
            rotation: regia_core::math::Quat::from_rotation_y(0.2)
                * regia_core::math::Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
            half_size: FVec3::new(1.7, 1.7, 1.2),
            color: [0.06, 0.05, 0.035, 0.95],
            lifetime: Some(45.0),
        });
        shakes.write(CameraShakeRequest {
            position,
            strength: 0.8,
            radius: 30.0,
        });
    }
    if name == "impact" {
        decals.write(SpawnDecal {
            position: FVec3::new(0.3, 0.0, 0.1),
            rotation: regia_core::math::Quat::from_rotation_y(-0.4)
                * regia_core::math::Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
            half_size: FVec3::new(0.5, 0.5, 0.8),
            color: [0.2, 0.12, 0.06, 0.85],
            lifetime: Some(20.0),
        });
    }
    let _ = shake;
}

/// Update: the fly-by trail rig.
fn trail_director(
    time: Res<Time>,
    cameras: Query<&Transform, With<Camera>>,
    mut trails: ResMut<VfxTrails>,
    mut director: ResMut<Director>,
) {
    let t = director.time;
    let a = t * 0.45 * std::f32::consts::TAU;
    let (s, c) = a.sin_cos();
    let pos = FVec3::new(2.6 * s, 1.9 + 0.4 * s, 2.2 * s * c);
    let Some(rig) = director.trail.as_mut() else {
        return;
    };
    rig.update(pos, time.delta_secs());
    // Face the camera with the first camera's right axis.
    let right = cameras
        .single()
        .map(|transform| {
            let r = transform.rotation * -Vec3::Z;
            FVec3::new(r.x, 0.0, r.z).normalize_or_zero().cross(FVec3::Y) * -1.0
        })
        .unwrap_or(FVec3::X);
    let mut verts = std::mem::take(&mut trails.0);
    rig.build_vertices(&mut verts, right, [1.0, 0.7, 0.3, 1.0]);
    trails.0 = verts;
    let _ = t;
}

/// Update: camera shake applied to the camera entity's transform.
fn apply_shake(mut cameras: Query<&mut Transform, With<Camera>>, shake: Res<regia_vfx::CameraShake>) {
    let offset = shake.offset();
    if offset.length_squared() < 1.0e-8 {
        return;
    }
    let roll = shake.roll();
    for mut transform in &mut cameras {
        transform.translation += Vec3::new(offset.x, offset.y, offset.z);
        if roll.abs() > 1.0e-6 {
            let forward = (transform.rotation * -Vec3::Z).normalize_or_zero();
            transform.rotation = Quat::from_axis_angle(forward, roll) * transform.rotation;
        }
    }
}

/// Last: frame counting, screenshot capture and exit.
fn frame_tick(
    time: Res<Time>,
    plan: Res<CapturePlan>,
    taken: Res<ScreenshotTaken>,
    mut requests: ResMut<ScreenshotRequests>,
    mut exit: MessageWriter<AppExit>,
) {
    let frames = time.frame_count();
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
                    match crate::save_png(&path, &image.rgba, image.width, image.height) {
                        Ok(()) => tracing::info!("vfx_lab: screenshot saved to {}", path.display()),
                        Err(err) => tracing::error!("vfx_lab: screenshot save failed: {err}"),
                    }
                    written.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Err(err) => tracing::error!("vfx_lab: screenshot failed: {err}"),
            }),
        );
    }

    if plan.exit_after_frames > 0 && frames + 1 >= u64::from(plan.exit_after_frames) {
        let written = taken.0.load(std::sync::atomic::Ordering::SeqCst);
        if plan.screenshot.as_ref().is_none_or(|_| written) {
            tracing::info!("vfx_lab: exit after {} frames", plan.exit_after_frames);
            exit.write(AppExit::Success);
        }
    }
}

// ---------------------------------------------------------------------------
// Procedural meshes (the sample ships no assets; M4's cook path is not used here)
// ---------------------------------------------------------------------------

/// A ground grid: `segments + 1`² vertices, `segments`² quads, CCW from +Y.
fn ground_vertices(extent: f32, segments: u32) -> Vec<regia_render::LabVertex> {
    let step = extent / segments as f32;
    let half = extent * 0.5;
    let mut verts = Vec::with_capacity((segments as usize + 1).pow(2));
    for row in 0..=segments {
        for col in 0..=segments {
            let x = -half + col as f32 * step;
            let z = -half + row as f32 * step;
            verts.push(regia_render::LabVertex {
                position: [x, 0.0, z],
                normal: [0.0, 1.0, 0.0],
                uv: [col as f32 * 0.25, row as f32 * 0.25],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0, 1.0, 1.0, 1.0],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    verts
}

fn ground_indices(segments: u32) -> Vec<u32> {
    let mut indices = Vec::with_capacity(segments as usize * segments as usize * 6);
    for row in 0..segments {
        for col in 0..segments {
            let a = row * (segments + 1) + col;
            let b = a + 1;
            let c = a + segments + 1;
            let d = c + 1;
            indices.extend_from_slice(&[a, d, b, a, c, d]);
        }
    }
    indices
}

/// An axis-aligned box centred on the origin (24 vertices, flat normals).
///
/// Each face is centred on its ±axis at the face's half-extent and spans the true
/// half-extents along its two in-plane axes (`tu × tv = n`, so the `-1..1` quad order
/// winds CCW seen from outside, matching the ground grid's +Y convention).
pub(crate) fn box_vertices(hx: f32, hy: f32, hz: f32) -> Vec<regia_render::LabVertex> {
    let mut verts = Vec::with_capacity(24);
    /// One box face: outward normal, in-plane axes (`tu × tv = n`) and the
    /// half-extents they span.
    struct Face {
        n: [f32; 3],
        tu: [f32; 3],
        tv: [f32; 3],
        hu: f32,
        hv: f32,
    }
    let faces = [
        Face {
            n: [0.0, 0.0, 1.0],
            tu: [1.0, 0.0, 0.0],
            tv: [0.0, 1.0, 0.0],
            hu: hx,
            hv: hy,
        }, // +Z
        Face {
            n: [0.0, 0.0, -1.0],
            tu: [-1.0, 0.0, 0.0],
            tv: [0.0, 1.0, 0.0],
            hu: hx,
            hv: hy,
        }, // -Z
        Face {
            n: [1.0, 0.0, 0.0],
            tu: [0.0, 0.0, -1.0],
            tv: [0.0, 1.0, 0.0],
            hu: hz,
            hv: hy,
        }, // +X
        Face {
            n: [-1.0, 0.0, 0.0],
            tu: [0.0, 0.0, 1.0],
            tv: [0.0, 1.0, 0.0],
            hu: hz,
            hv: hy,
        }, // -X
        Face {
            n: [0.0, 1.0, 0.0],
            tu: [1.0, 0.0, 0.0],
            tv: [0.0, 0.0, -1.0],
            hu: hx,
            hv: hz,
        }, // +Y
        Face {
            n: [0.0, -1.0, 0.0],
            tu: [1.0, 0.0, 0.0],
            tv: [0.0, 0.0, 1.0],
            hu: hx,
            hv: hz,
        }, // -Y
    ];
    for Face {
        n: normal,
        tu,
        tv,
        hu,
        hv,
    } in faces
    {
        // The face plane sits at the half-extent along the normal.
        let h_n = normal[0].abs() * hx + normal[1].abs() * hy + normal[2].abs() * hz;
        let quad = [[-1.0f32, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
        for [u, v] in quad {
            verts.push(regia_render::LabVertex {
                position: [
                    normal[0] * h_n + tu[0] * u * hu + tv[0] * v * hv,
                    normal[1] * h_n + tu[1] * u * hu + tv[1] * v * hv,
                    normal[2] * h_n + tu[2] * u * hu + tv[2] * v * hv,
                ],
                normal,
                uv: [u * 0.5 + 0.5, v * 0.5 + 0.5],
                tangent: [tu[0], tu[1], tu[2], 1.0],
                color: [1.0, 1.0, 1.0, 1.0],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    verts
}

pub(crate) fn box_indices() -> Vec<u32> {
    // Six faces, four vertices each (0..3 CCW from outside), two triangles per face.
    let mut indices = Vec::with_capacity(36);
    for face in 0..6u32 {
        let a = face * 4;
        indices.extend_from_slice(&[a, a + 1, a + 2, a, a + 2, a + 3]);
    }
    indices
}

/// Sky material for the window dome.
///
/// Black albedo, roughness 1, ~18 000 nits of blue emissive. A black occlusion map
/// forces the IBL term to zero: at roughness 1 the split-sum specular
/// `pref * (f0 * scale + bias)` is still non-zero (the bias term), and it varies
/// with the reflection direction across the dome, which painted a full-width zenith
/// lobe lighter than the emissive sky. The sun's GGX does not reach the visible
/// dome — inward normals face away from the downward sun — so the shaded result is
/// the constant emissive colour.
fn sky_material() -> CpuMaterial {
    CpuMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [0.3, 0.45, 1.0],
        // ~2× `SKY_IBL_SCALE`: a bright clear sky (~18k nits) so enough blue survives
        // the EV15 exposure + AgX tonemap to read as sky.
        emissive_intensity: 18_000.0,
        // Fully rough: roughness 0.5 caught the 90 klux sun's GGX as a fan across the dome.
        roughness: 1.0,
        occlusion_strength: 1.0,
        occlusion_map: Some(LabTexture {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
        }),
        ..Default::default()
    }
}

/// An inward-facing hemisphere ("sky dome"): `sectors` bands around Y from the zenith
/// (ring 0, duplicated pole vertices) down to the horizon (ring `rings`). Normals point
/// toward the centre, so the inside of the dome faces the camera.
fn sky_dome_vertices(radius: f32, sectors: u32, rings: u32) -> Vec<regia_render::LabVertex> {
    let mut verts = Vec::with_capacity((sectors + 1) as usize * (rings + 1) as usize);
    for ring in 0..=rings {
        // ~2.4° below the equator: slightly-horizontal rays that pass over the 100 m
        // ground edge would otherwise show the clear colour in a thin horizon band.
        let phi = 1.045 * std::f32::consts::FRAC_PI_2 * ring as f32 / rings as f32;
        let (sin_phi, cos_phi) = phi.sin_cos();
        for sector in 0..=sectors {
            let theta = sector as f32 / sectors as f32 * std::f32::consts::TAU;
            let (sin_t, cos_t) = theta.sin_cos();
            let position = [
                radius * sin_phi * cos_t,
                radius * cos_phi,
                radius * sin_phi * sin_t,
            ];
            let inward = [
                -position[0] / radius,
                -position[1] / radius,
                -position[2] / radius,
            ];
            verts.push(regia_render::LabVertex {
                position,
                normal: inward,
                uv: [sector as f32 / sectors as f32, ring as f32 / rings as f32],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0, 1.0, 1.0, 1.0],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    verts
}

/// Triangle indices for [`sky_dome_vertices`], wound so the front faces point inward
/// (the camera sits inside the dome; back-face culling must not remove the sky).
fn sky_dome_indices(sectors: u32, rings: u32) -> Vec<u32> {
    let at = |ring: u32, sector: u32| ring * (sectors + 1) + sector;
    let mut indices = Vec::with_capacity(sectors as usize * rings as usize * 6);
    for ring in 0..rings {
        for sector in 0..sectors {
            let a = at(ring, sector);
            let b = at(ring, sector + 1);
            let c = at(ring + 1, sector);
            let d = at(ring + 1, sector + 1);
            // Inward winding: the outward order would be (a, b, d) / (a, d, c).
            indices.extend_from_slice(&[a, d, b, a, c, d]);
        }
    }
    indices
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference boxes must be closed boxes spanning exactly the given
    /// half-extents (the review defect: faces built as `corner + axes * ±1` made
    /// 2 m × 2 m off-centre cards for 1 × 0.6 × 1 half-extents — "large flat cards").
    #[test]
    fn box_vertices_form_a_closed_box_at_true_half_extents() {
        let (hx, hy, hz) = (1.0f32, 0.6f32, 1.0f32);
        let verts = box_vertices(hx, hy, hz);
        assert_eq!(verts.len(), 24, "six faces × four vertices");

        // All 8 corners present, and nothing beyond the AABB (fully closed surface).
        for cx in [-hx, hx] {
            for cy in [-hy, hy] {
                for cz in [-hz, hz] {
                    let found = verts.iter().any(|v| {
                        (v.position[0] - cx).abs() < 1e-5
                            && (v.position[1] - cy).abs() < 1e-5
                            && (v.position[2] - cz).abs() < 1e-5
                    });
                    assert!(found, "corner ({cx},{cy},{cz}) missing");
                }
            }
        }
        for v in &verts {
            assert!(v.position[0].abs() <= hx + 1e-5, "x beyond half-extent");
            assert!(v.position[1].abs() <= hy + 1e-5, "y beyond half-extent");
            assert!(v.position[2].abs() <= hz + 1e-5, "z beyond half-extent");
            // Every vertex lies on the surface: one axis clamped to its half-extent.
            let on_surface = (v.position[0].abs() - hx).abs() < 1e-5
                || (v.position[1].abs() - hy).abs() < 1e-5
                || (v.position[2].abs() - hz).abs() < 1e-5;
            assert!(on_surface, "vertex inside the box: {:?}", v.position);
            // Flat normals are unit length and axis-aligned.
            let len: f32 = v.normal.iter().map(|n| n * n).sum::<f32>().sqrt();
            assert!((len - 1.0).abs() < 1e-5, "normal not unit: {:?}", v.normal);
            let axes = v.normal.iter().filter(|n| n.abs() > 1e-5).count();
            assert_eq!(axes, 1, "normal not axis-aligned: {:?}", v.normal);
        }

        // Each triangle's geometric normal (right-hand rule) points out of the box:
        // the winding is CCW seen from outside, like the ground grid from +Y.
        let indices = box_indices();
        assert_eq!(indices.len(), 36, "six faces × two triangles");
        for tri in indices.as_chunks::<3>().0 {
            let a = verts[tri[0] as usize].position;
            let b = verts[tri[1] as usize].position;
            let c = verts[tri[2] as usize].position;
            let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let n = [
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            ];
            // The centroid of the triangle must move along the normal (outward).
            let centroid = [
                (a[0] + b[0] + c[0]) / 3.0,
                (a[1] + b[1] + c[1]) / 3.0,
                (a[2] + b[2] + c[2]) / 3.0,
            ];
            let outward = n[0] * centroid[0] / hx + n[1] * centroid[1] / hy + n[2] * centroid[2] / hz;
            assert!(outward > 0.0, "triangle winds inward at centroid {centroid:?}");
        }
    }

    /// The window sky stays an emissive dielectric: black albedo, ~18 000 nits,
    /// roughness 1, and a black occlusion map so the IBL specular term is zero.
    /// `unlit` is false — that path returns base colour and drops the emissive.
    #[test]
    fn sky_material_is_emissive_with_ibl_occluded() {
        let mat = sky_material();
        assert_eq!(mat.base_color, [0.0, 0.0, 0.0, 1.0]);
        assert!((mat.emissive_intensity - 18_000.0).abs() < 1.0);
        assert!((mat.roughness - 1.0).abs() < 1e-6);
        assert!(!mat.unlit, "unlit returns base color and drops emissive");
        assert!((mat.occlusion_strength - 1.0).abs() < 1e-6);
        let occ = mat.occlusion_map.as_ref();
        assert!(
            occ.is_some_and(|tex| {
                tex.width == 1 && tex.height == 1 && tex.rgba.iter().step_by(4).all(|texel| *texel == 0)
            }),
            "sky occlusion map must be a black texel"
        );
    }

    /// The sky dome is a hemisphere (upper half only) with inward-facing triangles.
    #[test]
    fn sky_dome_is_an_inward_hemisphere() {
        let (radius, sectors, rings) = (300.0f32, 24u32, 12u32);
        let verts = sky_dome_vertices(radius, sectors, rings);
        assert_eq!(verts.len(), ((sectors + 1) * (rings + 1)) as usize);
        for v in &verts {
            let len = (v.position[0] * v.position[0]
                + v.position[1] * v.position[1]
                + v.position[2] * v.position[2])
                .sqrt();
            assert!((len - radius).abs() < 1e-2, "vertex off the dome: {len}");
            // The dome spans zenith .. slightly below the equator (the horizon skirt
            // covers the band where near-horizontal rays pass over the ground edge):
            // 1.045 · π/2 ≈ 94°, cos 94° ≈ −0.07.
            assert!(
                v.position[1] >= -0.08 * radius && v.position[1] <= radius,
                "vertex outside the dome band: {:?}",
                v.position
            );
        }
        // Indices reference existing vertices only.
        for i in sky_dome_indices(sectors, rings) {
            assert!((i as usize) < verts.len(), "index out of range");
        }
    }
}
