// SPDX-License-Identifier: MPL-2.0
//! The ray-traced sun on a terrain with scatter (ADR-2310, `book/agents/hwrt.md`): the terrain hands
//! the trace its caster proxy, so `on` no longer falls back to the screen-space trace on a frame
//! with terrain. A device that did not ask for ray queries (the default one, on lavapipe too) keeps the
//! screen fallback for `on`, and the compute reference traces the same scene. The ray-query
//! frames run where the adapter has ray queries and print `skipped: no ray queries on <adapter>`
//! where it does not (`regia_test_utils::hwrt`; `book/agents/hwrt.md` has the commands).

use std::sync::OnceLock;

use regia_render::hwrt::{RtShadowRequest, SunShadowReason, SunShadowVariant};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, RenderDevice,
};
use regia_terrain::scatter::rock::{RockStyle, procedural_rock};
use regia_terrain::{
    CdlodConfig, LayerDesc, TerrainLayers, TerrainMap, TerrainRelief, TerrainSharedDraw, TraceProxy,
    TraceProxySettings,
};

const W: u32 = 160;
const H: u32 = 96;
const SAMPLES: u32 = 129;
const WORLD: f32 = 512.0;
/// Rocks scattered along the ridge's foot.
const ROCKS: usize = 6;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// A ridge along Z west of the camera's target, with gentle roll everywhere.
fn height(x: f32, z: f32) -> f32 {
    let ridge = 40.0 * (-((x + 60.0) / 25.0).powi(2)).exp();
    ridge + 2.0 * (x / 23.0).sin() * (z / 31.0).cos()
}

fn map() -> TerrainMap {
    let spacing = WORLD / (SAMPLES - 1) as f32;
    let half = WORLD * 0.5;
    let heights: Vec<f32> = (0..SAMPLES)
        .flat_map(|j| (0..SAMPLES).map(move |i| (i, j)))
        .map(|(i, j)| height(i as f32 * spacing - half, j as f32 * spacing - half))
        .collect();
    TerrainMap::from_heights(SAMPLES, SAMPLES, heights, WORLD, 120.0, -60.0).expect("map")
}

fn ground(device: &RenderDevice) -> TerrainSharedDraw {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [0.4, 0.4, 0.4],
            roughness: 1.0,
            tiling: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let mut draw = TerrainSharedDraw::new(device, map(), &stack, CdlodConfig::default(), 4).expect("terrain");
    draw.set_relief(TerrainRelief::off());
    draw
}

/// The scene: rocks on the ground east of the ridge, the low sun from the west (the ridge's shadow
/// falls on the rocks' ground), `moved` metres added to the first rock's x.
fn scene(moved: f32) -> LabScene {
    let rock = procedural_rock(7, RockStyle::angular([0.45, 0.42, 0.4]));
    let lod0 = &rock.lods[0];
    let meshes = (0..ROCKS)
        .map(|i| {
            let x = -20.0 + 4.0 * i as f32 + if i == 0 { moved } else { 0.0 };
            let z = 6.0 * (i % 2) as f32;
            let model = regia_core::glam::Mat4::from_scale_rotation_translation(
                regia_core::glam::Vec3::splat(2.0),
                regia_core::glam::Quat::IDENTITY,
                regia_core::glam::Vec3::new(x, height(x, z), z),
            );
            LabMesh {
                vertices: lod0.vertices.clone(),
                indices: lod0.indices.clone(),
                material: 0,
                model: model.to_cols_array_2d(),
                ..LabMesh::unskinned()
            }
        })
        .collect();
    LabScene {
        width: W,
        height: H,
        eye: [10.0, 30.0, 40.0],
        target: [-15.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 1000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([1.0, -0.45, 0.2], [1.0; 3], 80_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.45, 0.42, 0.4, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes,
    }
}

fn quality(request: RtShadowRequest) -> FrameQuality {
    let mut quality = FrameQuality::LAB;
    quality.sun_shadow_trace = request;
    quality
}

fn frame(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    moved: f32,
    request: RtShadowRequest,
) -> Vec<u8> {
    let lab = scene(moved);
    let frame = lab.as_frame_scene_with_quality(quality(request));
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

fn renderer(device: &RenderDevice) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(ground(device))));
    renderer
}

/// Tiles of the proxy a default terrain builds over this map.
fn proxy_tiles() -> u32 {
    TraceProxy::build(&map(), TraceProxySettings::default())
        .stats()
        .tiles
}

/// A device that did not ask for ray queries (the default one): `on` keeps the old fallback, the
/// screen trace on the shadow map, and the reason is the device's, not the terrain's.
#[test]
fn on_without_ray_queries_keeps_the_screen_fallback_on_terrain() {
    let device = device();
    let mut renderer = renderer(device);
    frame(device, &mut renderer, 0.0, RtShadowRequest::On);
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::ScreenTrace);
    assert_eq!(report.selection.reason, SunShadowReason::ScreenFallback);
    assert_eq!(report.selection.reason.code(), "rt_shadows_screen_fallback");
}

/// The compute reference traces the terrain proxy and the rocks: the scene's instance count is the
/// rocks plus the proxy's tiles, and a steady frame rebuilds nothing.
#[test]
fn the_compute_reference_traces_terrain_and_scatter() {
    let device = device();
    let mut renderer = renderer(device);
    frame(device, &mut renderer, 0.0, RtShadowRequest::Compute);
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::ComputeFallback);
    assert_eq!(report.failure, None);
    let tiles = proxy_tiles();
    assert!(tiles >= 4, "{tiles}");
    assert_eq!(report.shared_casters, tiles);
    assert_eq!(report.casters, ROCKS as u32 + tiles);
    assert_eq!(report.scene_builds, 1);
    frame(device, &mut renderer, 0.0, RtShadowRequest::Compute);
    assert_eq!(renderer.sun_trace_report().scene_builds, 1, "nothing moved");
}

/// The knob off on a terrain frame draws what a renderer that never traced draws.
#[test]
fn the_knob_off_is_the_default_terrain_frame() {
    let device = device();
    let mut plain = renderer(device);
    let reference = frame(device, &mut plain, 0.0, RtShadowRequest::Off);
    let mut traced = renderer(device);
    frame(device, &mut traced, 0.0, RtShadowRequest::Compute);
    let back = frame(device, &mut traced, 0.0, RtShadowRequest::Off);
    assert_eq!(back, reference);
}

/// On a card with ray queries: a terrain + scatter scene traces with hardware ray queries (no
/// screen fallback), the TLAS holds every rock and proxy tile, and a rock that moves for 100
/// frames refits the top level with the instance count constant.
#[test]
fn ray_queries_trace_terrain_and_scatter_and_refit() {
    let Some(device) = regia_test_utils::hwrt::ray_query_device() else {
        return;
    };
    let mut renderer = renderer(&device);
    frame(&device, &mut renderer, 0.0, RtShadowRequest::On);
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::RayQuery);
    assert_eq!(report.selection.reason.code(), "rt_shadows_ray_query");
    assert_eq!(report.failure, None);
    let tiles = proxy_tiles();
    let instances = ROCKS + tiles as usize;
    assert_eq!(report.shared_casters, tiles);
    assert_eq!(report.accel.instances, instances);
    let builds = report.scene_builds;
    for i in 1..=100 {
        frame(&device, &mut renderer, i as f32 * 0.02, RtShadowRequest::On);
        let report = renderer.sun_trace_report();
        assert_eq!(report.accel.instances, instances, "frame {i}");
        assert!(report.accel.refit, "frame {i} refits");
    }
    let report = renderer.sun_trace_report();
    assert_eq!(report.scene_builds, builds);
    assert_eq!(report.tlas_refits, 100);
}

/// What the traced sun costs on a terrain frame on this adapter, printed (`--nocapture`), never
/// asserted: 1080p, GPU timing on, the camera moving. For `docs/perf-log.md`.
#[test]
#[ignore = "a measurement for a real adapter (book/agents/hwrt.md)"]
fn terrain_trace_cost_on_this_adapter() {
    let device = RenderDevice::headless_with_ray_queries().expect("device");
    let has_rq = device
        .device
        .features()
        .contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY);
    println!("adapter: {}", device.adapter.get_info().name);
    let mut requests = vec![RtShadowRequest::Off, RtShadowRequest::Screen];
    if has_rq {
        requests.push(RtShadowRequest::On);
    }
    for request in requests {
        let mut renderer = renderer(&device);
        renderer.set_gpu_timing(&device, true);
        let mut lab = scene(0.0);
        lab.width = 1920;
        lab.height = 1080;
        let start = std::time::Instant::now();
        let first = lab.as_frame_scene_with_quality(quality(request));
        renderer.render_offscreen(&device, &first).expect("first");
        let first_ms = start.elapsed().as_secs_f32() * 1000.0;
        let frames = 24;
        let (mut trace_ms, mut forward_ms, mut shadow_ms, mut wall_ms) = (0.0, 0.0, 0.0, 0.0);
        for i in 0..frames {
            let mut moved = lab.clone();
            moved.eye[0] += 0.05 * (i + 1) as f32;
            let frame = moved.as_frame_scene_with_quality(quality(request));
            let start = std::time::Instant::now();
            renderer.render_offscreen(&device, &frame).expect("frame");
            if i >= frames / 2 {
                wall_ms += start.elapsed().as_secs_f32() * 1000.0;
                let gpu = renderer.gpu_timings();
                trace_ms += gpu.contact_shadows_ms;
                forward_ms += gpu.forward_ms;
                shadow_ms += gpu.shadows_ms;
            }
        }
        let counted = (frames - frames / 2) as f32;
        let report = renderer.sun_trace_report();
        println!(
            "{:>8} {:>16} first {first_ms:9.1} ms, frame (wall) {:8.2} ms, trace {:7.3} ms, shadow maps {:7.3} ms, forward {:7.3} ms, casters {}, shared {}, tris {}, maps skipped {}",
            request.as_str(),
            report.selection.variant.as_str(),
            wall_ms / counted,
            trace_ms / counted,
            shadow_ms / counted,
            forward_ms / counted,
            report.casters,
            report.shared_casters,
            report.triangles,
            report.sun_maps_skipped,
        );
    }
}
