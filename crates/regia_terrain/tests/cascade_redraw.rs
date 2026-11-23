// SPDX-License-Identifier: MPL-2.0
//! Sun cascades on a moving camera over canyon relief (lane terrain-perf).
//!
//! A 60-frame strafe at 15 m/s (0.25 m per 60 Hz frame) along the floor of a mesa map, on
//! Desktop High with paged sun shadows. Before this lane every cascade was refitted to the
//! camera's frustum slice each frame (one-texel snapping) and the terrain's single shadow
//! key changed with every LOD split, so all four cascades were cleared and redrawn on
//! nearly every frame. Now a cascade keeps its placement while it still covers its
//! receivers and its node list is selected from an anchored eye: the strafe redraws a
//! handful of cascades in total, and the last frame matches a fresh renderer's single
//! frame of the same camera.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, OutdoorParams, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw, VolumetricFogSettings,
};
use regia_terrain::noise::fbm2;
use regia_terrain::{CdlodConfig, TerrainLayers, TerrainMap, TerrainSharedDraw};

const W: u32 = 128;
const H: u32 = 72;
const WORLD_M: f32 = 1280.0;
const FRAMES: u32 = 60;
/// 15 m/s at 60 Hz.
const STEP_M: f32 = 0.25;
/// Frames both renderers hold the final camera before the comparison.
const HOLD: u32 = 16;

fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Stepped mesas over a valley floor (the shape of DO27's Switchbacks).
fn canyon_map() -> TerrainMap {
    let n = 1025u32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        let z = row as f32 / (n - 1) as f32 * WORLD_M - WORLD_M * 0.5;
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * WORLD_M - WORLD_M * 0.5;
            let mesa = fbm2(x / 230.0, z / 230.0, 61) * 0.75 + fbm2(x / 80.0, z / 80.0, 62) * 0.25;
            let raw = smooth(0.50, 0.62, mesa) * 95.0 + 25.0 * smooth(0.62, 0.9, mesa);
            let bed = (raw / 11.0).floor();
            let terraced = (bed + smooth(0.55, 0.9, raw / 11.0 - bed)) * 11.0;
            heights.push(raw + (terraced - raw) * 0.3 + 1.2 * (fbm2(x / 9.0, z / 9.0, 63) - 0.5));
        }
    }
    TerrainMap::from_heights(n, n, heights, WORLD_M, 1.0, 0.0).expect("canyon map")
}

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"
        TerrainLayers(
            version: 1,
            wetness_default: 0.0,
            layers: [
                LayerDesc(
                    name: "dust",
                    albedo: (0.62, 0.40, 0.27),
                    roughness: 0.9,
                    tiling: 4.0,
                    slope_min: 0.0,
                    slope_max: 1.6,
                    altitude_min: -1000.0,
                    altitude_max: 5000.0,
                    blend_sharpness: 0.15,
                    triplanar: false,
                ),
            ],
        )
        "#,
    )
    .expect("layers")
}

fn post() -> PostSettings {
    PostSettings {
        exposure_ev: 13.3,
        taa: false,
        ..PostSettings::golden_deterministic()
    }
}

fn scene(map: &TerrainMap, frame: u32, post: &PostSettings) -> LabScene {
    // Sideways along +X on the valley floor, looking north-east across it at a mesa flank.
    let x = 185.0 + frame as f32 * STEP_M;
    let z = -265.0;
    let ground = map.sample_bilinear(x, z);
    let target = [x + 145.0, 0.0, z + 205.0];
    let target_ground = map.sample_bilinear(target[0], target[2]);
    LabScene {
        width: W,
        height: H,
        eye: [x, ground + 6.0, z],
        target: [target[0], target_ground + 15.0, target[2]],
        fov_y: 0.9,
        near: 0.1,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 13.3,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.55, -0.62, -0.56], [1.0, 0.88, 0.72], 52_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    }
}

fn renderer(device: &RenderDevice, map: &TerrainMap) -> FrameRenderer {
    let mut draw =
        TerrainSharedDraw::new(device, map.clone(), &layers(), CdlodConfig::default(), 6).expect("terrain");
    draw.prewarm(wgpu::TextureFormat::Rgba16Float);
    let mut renderer = FrameRenderer::new();
    // These tests compare a held or strafed frame with a fresh renderer's frame; the
    // irradiance probes converge over frames, so they stay off here (probe stability has
    // its own tests in regia_render).
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    renderer
}

fn render(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    post: &PostSettings,
) -> Vec<u8> {
    render_with(device, renderer, lab, post, None)
}

/// One frame; `vehicle` moves the scene's first mesh to that translation.
fn render_with(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    post: &PostSettings,
    vehicle: Option<[f32; 3]>,
) -> Vec<u8> {
    // Desktop High (paged sun shadows) without the temporal resolve, so a frame depends on
    // the camera and the shadow maps, not on the path the camera took.
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(post);
    if let (Some(at), Some(mesh)) = (vehicle, frame.meshes.first_mut()) {
        mesh.model = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [at[0], at[1], at[2], 1.0],
        ];
    }
    renderer
        .render_offscreen(device, &frame)
        .unwrap_or_else(|err| panic!("frame: {err}"))
        .pixels
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_strafe_over_canyon_relief_redraws_few_cascades_and_keeps_the_image() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let map = canyon_map();
    let post = post();
    let mut strafing = renderer(&device, &map);
    let mut last = Vec::new();
    let mut per_frame = Vec::new();
    let mut previous = 0;
    for frame in 0..FRAMES {
        last = render(&device, &mut strafing, &scene(&map, frame, &post), &post);
        let draws = strafing.shared_shadow_draws();
        if frame > 0 {
            per_frame.push(draws - previous);
        }
        previous = draws;
    }
    let total: u64 = per_frame.iter().sum();
    let worst = per_frame.iter().copied().max().unwrap_or(0);
    eprintln!(
        "strafe: {total} terrain cascade draws over {} frames (worst frame {worst}); {} page-region draws",
        per_frame.len(),
        strafing.shared_shadow_region_draws()
    );
    // Four cascades refitted every frame was 4 × 59. A re-placement or a reselection of
    // one cascade is one draw; a 15 m strafe needs a few of them.
    assert!(total <= 8, "{total} terrain cascade draws over a 15 m strafe");
    assert!(worst <= 2, "one frame redrew {worst} terrain cascades");
    assert_eq!(strafing.shared_shadow_region_draws(), 0, "no mesh moved");

    // The kept placements share the fresh fit's texel lattice; what may differ is the
    // cascade mesh's LOD, selected up to one shadow node behind the camera. Both renderers
    // hold the final camera until the temporal resolve settles (the strafe's motion blur
    // and TAA history are not what is compared).
    let held = scene(&map, FRAMES - 1, &post);
    for _ in 0..HOLD {
        last = render(&device, &mut strafing, &held, &post);
    }
    let mut fresh = renderer(&device, &map);
    let mut reference = Vec::new();
    for _ in 0..=HOLD {
        reference = render(&device, &mut fresh, &held, &post);
    }
    let (mean, over) = compare(&last, &reference);
    eprintln!("last strafe frame vs fresh frame: mean |Δ| {mean:.3}, {over} pixels over 8");
    assert!(
        mean < 0.5,
        "the strafed frame drifted from a fresh one: mean |Δ| {mean:.3}"
    );
    assert!(
        over * 200 <= (W * H) as usize,
        "{over} pixels differ by more than 8 from a fresh frame"
    );
}

#[test]
fn a_strafe_through_volumetric_fog_keeps_the_near_cascade() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Volumetric fog reads one cascade at every froxel. Cascade 0 used to keep the fresh
    // fit whenever fog ran, so on DO27's hazy Switchbacks chase it was re-placed and the
    // terrain redrawn into it on nearly every frame (157 of 180 frames). Fog now reads the
    // first cascade that reaches 150 m and fades its shadow toward the cascade edge, and
    // every cascade keeps its placement.
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let map = canyon_map();
    let post = post();
    let mut strafing = renderer(&device, &map);
    strafing.set_outdoor(OutdoorParams {
        fog: VolumetricFogSettings {
            density: 2.0e-3,
            height_falloff: 0.01,
            base_height: 0.0,
            ..VolumetricFogSettings::default()
        },
        ..OutdoorParams::default()
    });
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.volumetric_fog = true;
    let mut per_frame = Vec::new();
    let mut previous = 0;
    for frame in 0..FRAMES {
        let lab = scene(&map, frame, &post);
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&post);
        strafing
            .render_offscreen(&device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        let draws = strafing.shared_shadow_draws();
        if frame > 0 {
            per_frame.push(draws - previous);
        }
        previous = draws;
    }
    let total: u64 = per_frame.iter().sum();
    eprintln!(
        "fog strafe: {total} terrain cascade draws over {} frames",
        per_frame.len()
    );
    // Cascade 0 refitted every frame was at least 59 draws.
    assert!(
        total <= 8,
        "{total} terrain cascade draws over a 15 m strafe in fog"
    );
}

fn compare(a: &[u8], b: &[u8]) -> (f64, usize) {
    let mut sum = 0u64;
    let mut over = 0usize;
    for (a, b) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = (0..3)
            .map(|c| i32::from(a[c]).abs_diff(i32::from(b[c])))
            .max()
            .unwrap_or(0);
        sum += u64::from(d);
        if d > 8 {
            over += 1;
        }
    }
    (sum as f64 / f64::from(W * H), over)
}

const SUN: [f32; 3] = [-0.55, -0.62, -0.56];

/// `true` when the terrain blocks the sun at `(x, z)` (march toward the sun).
fn in_terrain_shadow(map: &TerrainMap, x: f32, z: f32) -> bool {
    let l = (SUN[0] * SUN[0] + SUN[1] * SUN[1] + SUN[2] * SUN[2]).sqrt();
    let to_sun = [-SUN[0] / l, -SUN[1] / l, -SUN[2] / l];
    let y0 = map.sample_bilinear(x, z) + 0.5;
    (1..200).any(|i| {
        let t = i as f32 * 2.0;
        let (px, pz) = (x + to_sun[0] * t, z + to_sun[2] * t);
        px.abs() < WORLD_M * 0.5
            && pz.abs() < WORLD_M * 0.5
            && map.sample_bilinear(px, pz) > y0 + to_sun[1] * t
    })
}

/// A point on the valley floor at the edge of a mesa's cast shadow: shadowed, with sunlit
/// ground 8 m further along +X.
fn shadow_edge(map: &TerrainMap) -> [f32; 2] {
    for zi in -60..60 {
        for xi in -60..60 {
            let (x, z) = (xi as f32 * 8.0, zi as f32 * 8.0);
            if map.sample_bilinear(x, z) < 6.0
                && in_terrain_shadow(map, x, z)
                && !in_terrain_shadow(map, x + 8.0, z)
                && !in_terrain_shadow(map, x + 16.0, z)
            {
                return [x, z];
            }
        }
    }
    panic!("no shadow edge on the canyon map");
}

/// A vehicle box driving out of a mesa's cast shadow, the camera chasing it. The moving
/// box dirties a few shadow pages per frame; the terrain is redrawn only under those pages
/// (with only the nodes there), and the frame must still match a fresh one — the mesa's
/// shadow on the ground around the vehicle included.
#[test]
fn a_moving_caster_redraws_the_terrain_only_under_its_pages() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let map = canyon_map();
    let post = post();
    let edge = shadow_edge(&map);
    let (verts, indices) = regia_render::box_mesh([2.1, 0.8, 1.0]);
    let verts = regia_render::translate_mesh(&verts, [0.0, 0.8, 0.0]);
    let mut lab = scene(&map, 0, &post);
    lab.directional = DirectionalLightUniform::new(SUN, [1.0, 0.88, 0.72], 52_000.0);
    lab.materials = vec![regia_render::LabMaterial::default()];
    lab.meshes = vec![regia_render::LabMesh {
        vertices: verts,
        indices,
        material: 0,
        ..regia_render::LabMesh::unskinned()
    }];
    // Vehicle along +X through the shadow edge; chase camera 9 m behind at a fixed height
    // 3.5 m over the edge (and clear of the ground under its path).
    let start = edge[0] - 7.5;
    let clear = (0..=FRAMES)
        .map(|f| map.sample_bilinear(start - 9.0 + f as f32 * STEP_M, edge[1]) + 1.5)
        .fold(map.sample_bilinear(edge[0], edge[1]) + 3.5, f32::max);
    let pose = |frame: u32| {
        let x = start + frame as f32 * STEP_M;
        let at = [x, map.sample_bilinear(x, edge[1]), edge[1]];
        ([x - 9.0, clear, edge[1]], [at[0], at[1] + 1.0, at[2]], at)
    };
    let mut strafing = renderer(&device, &map);
    let mut last = Vec::new();
    for frame in 0..FRAMES {
        let (eye, target, at) = pose(frame);
        lab.eye = eye;
        lab.target = target;
        last = render_with(&device, &mut strafing, &lab, &post, Some(at));
    }
    let full = strafing.shared_shadow_draws();
    let regions = strafing.shared_shadow_region_draws();
    eprintln!("vehicle chase: {full} whole-cascade terrain draws, {regions} page-region draws");
    assert!(regions > 0, "the moving vehicle never dirtied a page");
    assert!(full <= 4 + 8, "{full} whole-cascade terrain draws");

    let (eye, target, at) = pose(FRAMES - 1);
    lab.eye = eye;
    lab.target = target;
    let mut fresh = renderer(&device, &map);
    let reference = render_with(&device, &mut fresh, &lab, &post, Some(at));
    let (mean, over) = compare(&last, &reference);
    eprintln!("last vehicle frame vs fresh frame: mean |Δ| {mean:.3}, {over} pixels over 8");
    assert!(mean < 0.5, "mean |Δ| {mean:.3}");
    assert!(
        over * 200 <= (W * H) as usize,
        "{over} pixels differ by more than 8"
    );
}
