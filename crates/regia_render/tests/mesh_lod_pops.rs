// SPDX-License-Identifier: MPL-2.0
//! Mesh LOD cross-fade on a short dolly (v020-lod-select).
//!
//! One rigid prop, two index ranges over the same vertices (a quad, then a much smaller quad).
//! The camera backs through the switch. A hard switch is a one-frame jump in the tiles the quad
//! covers; the 8-frame dither spreads that jump. Scored against a run that never switches.
//! A second test checks a shadow-only mesh key stays out of the colour pass.

use std::sync::Arc;

use regia_asset::projected_screen_size;
use regia_core::math::Vec3;
use regia_render::{
    AntiAliasing, Camera, CpuMaterial, CpuMesh, DirectionalLight, DirectionalLightUniform, DisableMeshLod,
    ExtractedFrame, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabVertex, LodViewport,
    MESH_LOD_FADE_FRAMES, MeshLodChain, MeshLodCrossfade, MeshLodLevel, RenderDevice, RenderSettings,
    SHADOW_LOD_MESH_TAG, ShadowCast, SsaoMode, extract_frame, select_mesh_lods,
};
use regia_scene::{GlobalTransform, Transform};

const W: u32 = 128;
const H: u32 = 64;
const FOV: f32 = 0.9;
const FRAMES: usize = 18;
const TILE: u32 = 16;
const POP_STEP: f32 = 4.0;

#[derive(Clone, Copy)]
enum Mode {
    Fixed,
    Hard,
    Faded,
}

fn place(pos: Vec3) -> (Transform, GlobalTransform) {
    let transform = Transform::from_translation(pos);
    (transform, GlobalTransform(transform.compute_affine()))
}

fn quad(x0: f32, y0: f32, x1: f32, y1: f32) -> [LabVertex; 4] {
    let n = [0.0, 0.0, 1.0];
    [
        LabVertex::from_pos_normal_uv([x0, y0, 0.0], n, [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([x1, y0, 0.0], n, [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([x1, y1, 0.0], n, [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([x0, y1, 0.0], n, [0.0, 1.0]),
    ]
}

fn prop() -> (Arc<CpuMesh>, MeshLodChain) {
    let mut vertices = Vec::new();
    vertices.extend(quad(-1.0, -1.0, 1.0, 1.0));
    vertices.extend(quad(-0.08, -0.08, 0.08, 0.08));
    let fine = vec![0, 1, 2, 0, 2, 3];
    let coarse = vec![4, 5, 6, 4, 6, 7];
    let mesh = Arc::new(CpuMesh {
        vertices,
        indices: fine.clone(),
    });
    let chain = MeshLodChain {
        levels: vec![
            MeshLodLevel {
                indices: Arc::new(fine),
                screen_size: 0.5,
            },
            MeshLodLevel {
                indices: Arc::new(coarse),
                screen_size: 0.0,
            },
        ],
        bounds_center: [0.0, 0.0, 0.0],
        bounds_radius: 1.0,
    };
    (mesh, chain)
}

fn distance_for(size: f32) -> f32 {
    let mut lo = 0.3f32;
    let mut hi = 80.0f32;
    for _ in 0..32 {
        let mid = 0.5 * (lo + hi);
        if projected_screen_size(1.0, mid, FOV) > size {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

fn settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(regia_render::RenderProfilePreset::DesktopHigh);
    settings.profile.gi_probes = false;
    settings.profile.screen_space_gi = false;
    settings.profile.ssr = false;
    settings.profile.volumetric_fog = false;
    settings.profile.volumetric_clouds = false;
    settings.profile.physical_atmosphere = false;
    settings.profile.gpu_driven = false;
    settings.profile.reflection_probes = false;
    settings.profile.bloom = false;
    settings.profile.auto_exposure = false;
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.depth_prepass = false;
    settings.profile.ssao = SsaoMode::Off;
    settings.profile.contact_shadows = false;
    settings.profile.motion_blur = false;
    settings.profile.depth_of_field = false;
    settings.post.bloom = false;
    settings.post.auto_exposure = false;
    settings.post.taa = false;
    settings.post.fxaa = false;
    settings.post.exposure_ev = 14.0;
    // The dolly is laid out for a switch at screen size 0.5: Desktop High's tier distance scale
    // (2, ADR-260) would move it near the end of the dolly. 1 and 0.5 select as before the scales.
    settings.profile.mesh_lod_distance_scale = 1.0;
    settings.profile.mesh_lod_shadow_scale = 0.5;
    settings
}

/// Renders the dolly. Returns per-frame luma and how many frames had a dither pair.
fn dolly(device: &RenderDevice, mode: Mode) -> (Vec<Vec<f32>>, u32) {
    use regia_app::prelude::*;
    let (mesh, chain) = prop();
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(settings());
    // Stored thresholds are for a 1080-row display. The picture is small; the selector still
    // uses 1080 so a screen size of 0.5 is the switch the dolly crosses.
    app.insert_resource(LodViewport {
        width: 1920,
        height: 1080,
    });
    if matches!(mode, Mode::Hard) {
        app.insert_resource(MeshLodCrossfade(false));
    }
    app.add_systems(Render, (extract_frame, select_mesh_lods).chain());
    let mut spawned = app.world_mut().spawn((
        place(Vec3::ZERO),
        regia_render::MeshRenderer::new(
            Arc::clone(&mesh),
            Arc::new(CpuMaterial {
                base_color: [0.85, 0.85, 0.85, 1.0],
                roughness: 1.0,
                ..CpuMaterial::default()
            }),
        ),
        chain,
    ));
    if matches!(mode, Mode::Fixed) {
        spawned.insert(DisableMeshLod);
    }
    let camera = app
        .world_mut()
        .spawn((
            place(Vec3::new(0.0, 0.0, distance_for(1.0))),
            Camera {
                fov_y: FOV,
                exposure_ev: Some(14.0),
                near: 0.1,
                far: 200.0,
                ..Camera::default()
            },
        ))
        .id();
    app.world_mut().spawn((
        place(Vec3::ZERO),
        DirectionalLight {
            direction: [0.0, -0.25, -1.0],
            color: [1.0, 0.98, 0.95],
            illuminance: 90_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));
    let mut renderer = FrameRenderer::new();
    let start = distance_for(1.0);
    let end = distance_for(0.15);
    let mut frames = Vec::with_capacity(FRAMES);
    let mut fading = 0u32;
    for i in 0..FRAMES {
        let t = i as f32 / (FRAMES - 1) as f32;
        let distance = start + (end - start) * t;
        let (transform, global) = place(Vec3::new(0.0, 0.0, distance));
        app.world_mut().entity_mut(camera).insert((transform, global));
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>().clone();
        if extracted.meshes.iter().any(|mesh| mesh.lod_dither != 0.0) {
            fading += 1;
        }
        let scene = extracted.frame_scene(0, W, H);
        let image = renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("frame {i}: {err}"));
        frames.push(
            image
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
                .collect(),
        );
    }
    (frames, fading)
}

fn tile_deviation(a: &[f32], b: &[f32]) -> Vec<f32> {
    let (tx, ty) = (W / TILE, H / TILE);
    let mut tiles = vec![0.0f32; (tx * ty) as usize];
    for y in 0..ty * TILE {
        for x in 0..tx * TILE {
            let i = (y * W + x) as usize;
            tiles[((y / TILE) * tx + x / TILE) as usize] += (a[i] - b[i]).abs() / (TILE * TILE) as f32;
        }
    }
    tiles
}

/// `(pop count, largest tile jump)`. A hard switch is one large jump; a cross-fade splits it.
fn pops(run: &[Vec<f32>], reference: &[Vec<f32>], step: f32) -> (u32, f32) {
    let deviation: Vec<Vec<f32>> = run
        .iter()
        .zip(reference)
        .map(|(a, b)| tile_deviation(a, b))
        .collect();
    let jumps: Vec<Vec<f32>> = deviation
        .windows(2)
        .map(|w| {
            w[1].iter()
                .zip(&w[0])
                .map(|(now, before)| (now - before).abs())
                .collect()
        })
        .collect();
    let mut count = 0;
    let mut max_jump = 0.0f32;
    for f in 0..jumps.len() {
        for t in 0..jumps[f].len() {
            max_jump = max_jump.max(jumps[f][t]);
            let mut around: Vec<f32> = (f.saturating_sub(4)..(f + 5).min(jumps.len()))
                .filter(|&g| g != f)
                .map(|g| jumps[g][t])
                .collect();
            around.sort_by(f32::total_cmp);
            let median = around.get(around.len() / 2).copied().unwrap_or(0.0);
            if jumps[f][t] > step && jumps[f][t] > 3.0 * median {
                count += 1;
            }
        }
    }
    (count, max_jump)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn cross_fade_does_not_pop_and_a_hard_switch_does() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let started = std::time::Instant::now();
    let (reference, fixed_fading) = dolly(&device, Mode::Fixed);
    let (hard, hard_fading) = dolly(&device, Mode::Hard);
    let (soft, soft_fading) = dolly(&device, Mode::Faded);
    let (hard_pops, hard_jump) = pops(&hard, &reference, POP_STEP);
    let (soft_pops, soft_jump) = pops(&soft, &reference, POP_STEP);
    eprintln!(
        "mesh LOD dolly, {FRAMES} frames: hard pops {hard_pops} (max jump {hard_jump:.2}), \
         faded pops {soft_pops} (max jump {soft_jump:.2}), fading frames fixed {fixed_fading} \
         hard {hard_fading} soft {soft_fading}, {:.1}s",
        started.elapsed().as_secs_f32()
    );
    assert_eq!(fixed_fading, 0);
    assert_eq!(hard_fading, 0, "hard switch must not dither");
    assert!(
        soft_fading >= MESH_LOD_FADE_FRAMES as u32,
        "the dolly should cross-fade for {MESH_LOD_FADE_FRAMES} frames, saw {soft_fading}"
    );
    assert!(
        hard_jump > POP_STEP,
        "a hard switch must move a tile by more than {POP_STEP}, saw {hard_jump:.2}"
    );
    // Coverage steps by 1/9, so the fade's largest step stays under the one-frame jump.
    // The dolly also moves the silhouette; that motion is in both runs and is not the pop.
    assert!(
        soft_jump * 3.0 < hard_jump,
        "cross-fade max jump {soft_jump:.2} should stay under a third of the hard switch's {hard_jump:.2}"
    );
}

#[test]
fn shadow_lod_key_is_not_in_the_colour_pass() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let vertices = quad(-0.6, 0.4, 0.6, 1.6);
    let material = LabMaterial {
        base_color: [0.7, 0.7, 0.7, 1.0],
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let model = regia_core::math::Mat4::IDENTITY.to_cols_array_2d();
    let mesh = |key: u64| FrameMesh {
        is_static: false,
        mesh_key: key,
        material_key: 1,
        object_key: 1,
        vertices: &vertices,
        indices: &[0, 1, 2, 0, 2, 3],
        material: &material,
        model,
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    };
    let scene = |key: u64| FrameScene {
        width: 64,
        height: 64,
        eye: [0.0, 1.2, 3.0],
        target: [0.0, 1.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 40.0,
        depth_prepass: false,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.2, -0.8, -0.4], [1.0, 1.0, 1.0], 80_000.0),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![mesh(key)],
    };
    let mut renderer = FrameRenderer::new();
    renderer
        .render_offscreen(&device, &scene(7))
        .expect("colour draw");
    assert!(renderer.draw_stats().drawn >= 1, "an on-screen mesh is drawn");
    renderer
        .render_offscreen(&device, &scene(7 | SHADOW_LOD_MESH_TAG))
        .expect("shadow-only draw");
    assert_eq!(
        renderer.draw_stats().drawn,
        0,
        "a shadow-LOD key stays out of the colour pass"
    );
    assert!(renderer.shadow_caster_count() >= 1, "the same draw still casts");
}
