// SPDX-License-Identifier: MPL-2.0
//! End-to-end checks of the visibility-buffer path against the forward path on one device.

use std::sync::Arc;

use super::{MeshletBuilder, MeshletMesh, VisBufferSettings};
use crate::lab_frame::{LabMaterial, LabTexture, LabVertex};
use crate::profile::{FrameQuality, RenderProfile, RenderProfilePreset};
use crate::{FrameMesh, FrameRenderer, FrameScene, PostSettings, ShadowCast};

const SIZE: u32 = 96;

struct Fixture {
    sphere: (Vec<LabVertex>, Vec<u32>),
    ground: (Vec<LabVertex>, Vec<u32>),
    wall: (Vec<LabVertex>, Vec<u32>),
    rock: LabMaterial,
    floor: LabMaterial,
    models: Vec<[[f32; 4]; 4]>,
}

fn checker(size: u32) -> LabTexture {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let on = ((x / 4) + (y / 4)) % 2 == 0;
            rgba.extend_from_slice(if on {
                &[200, 150, 110, 255]
            } else {
                &[70, 60, 55, 255]
            });
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

fn translate(x: f32, y: f32, z: f32, s: f32) -> [[f32; 4]; 4] {
    [
        [s, 0.0, 0.0, 0.0],
        [0.0, s, 0.0, 0.0],
        [0.0, 0.0, s, 0.0],
        [x, y, z, 1.0],
    ]
}

fn fixture(n: i32) -> Fixture {
    let mut models = Vec::new();
    for z in 0..n {
        for x in 0..n {
            let s = 0.35 + 0.1 * ((x + 2 * z) % 3) as f32;
            models.push(translate((x - n / 2) as f32 * 1.1, s * 0.6, -(z as f32) * 1.1, s));
        }
    }
    Fixture {
        sphere: crate::uv_sphere(1.0, 24, 12),
        ground: crate::box_mesh([20.0, 0.05, 20.0]),
        wall: crate::box_mesh([3.0, 2.0, 0.1]),
        rock: LabMaterial {
            albedo: Some(checker(32)),
            ..LabMaterial::default()
        },
        floor: LabMaterial::default(),
        models,
    }
}

/// Exposure (EV100) the fixture's shared post settings apply to every frame.
///
/// The scene is a ~100 klx sun over a flat 0.8-albedo ground with procedural-sky IBL, so its
/// radiance is outdoor daylight (the surfaces sit in the tens of thousands of cd/m²). The
/// default [`PostSettings::exposure_ev`] of 0 multiplies that by only `1 / 1.2`, so every frame
/// tonemaps to near-white and the forward/vis-buffer comparisons compare saturated pixels that
/// pass even when the shading differs. At EV 15 an 18 % grey sunlit surface prints ≈ 0.43 in
/// display space (middle grey lands ≈ 0.5), keeping the scene's mid-tones between 0.18 and 0.5
/// and its diffuse surfaces under the white clamp.
const EXPOSURE_EV: f32 = 15.0;

fn post() -> PostSettings {
    PostSettings {
        taa: false,
        auto_exposure: false,
        bloom: false,
        exposure_ev: EXPOSURE_EV,
        ..PostSettings::default()
    }
}

fn scene<'a>(f: &'a Fixture, post: &'a PostSettings, vis: bool, wall: bool) -> FrameScene<'a> {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.visibility_buffer = vis;
    let mesh =
        |key: u64, object: u64, data: &'a (Vec<LabVertex>, Vec<u32>), material: &'a LabMaterial, model| {
            FrameMesh {
                is_static: false,
                mesh_key: key,
                material_key: key + 1000,
                object_key: object,
                vertices: &data.0,
                indices: &data.1,
                material,
                model,
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        };
    let mut meshes = vec![mesh(1, 1, &f.ground, &f.floor, translate(0.0, -0.05, 0.0, 1.0))];
    if wall {
        meshes.push(mesh(3, 3, &f.wall, &f.floor, translate(0.0, 1.0, 1.5, 1.0)));
    }
    for (i, model) in f.models.iter().enumerate() {
        meshes.push(mesh(2, 100 + i as u64, &f.sphere, &f.rock, *model));
    }
    FrameScene {
        width: SIZE,
        height: SIZE,
        eye: [0.0, 2.5, 4.0],
        target: [0.0, 0.0, -3.0],
        fov_y: 1.0,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post),
        fog: crate::FogSettings::default(),
        directional: crate::DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 100_000.0),
        local_lights: &[],
        quality,
        meshes,
    }
}

fn single_level_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(MeshletMesh::single_level(&positions, indices))
    })
}

/// A renderer for the vis-buffer-vs-forward comparisons.
///
/// On a CPU adapter (Mesa lavapipe) the material sampler is isotropic. llvmpipe's anisotropic
/// filter takes its rate and level from the explicit `textureSampleGrad` gradients but spaces its
/// taps by the coordinate's implicit 2×2-quad derivatives (Mesa `lp_build_sample_aniso`), which a
/// full-screen resolve cannot give it: its quads span triangles and objects. Hardware filters
/// from the explicit gradients, and these tests run there with the shipped 8× sampler; on
/// lavapipe both paths compare under the same trilinear sampler.
fn comparison_renderer(device: &crate::RenderDevice) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    if device.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        renderer.set_material_anisotropy(1);
    }
    renderer
}

fn render(renderer: &mut FrameRenderer, device: &crate::RenderDevice, scene: &FrameScene<'_>) -> Vec<u8> {
    renderer.render_offscreen(device, scene).expect("frame").pixels
}

/// The "close to the forward frame" floor: `strict` on the software rasterizer it was measured on.
/// Hardware rasterizes the visibility buffer and the forward draw with different edge and depth
/// rounding on Metal, so a few edge pixels differ by more than 2 levels (98.5 to 98.9 % close on
/// Apple M5, ADR-275); the floor there is 97 %, which a wrong or blank frame still fails. Vulkan
/// adapters (lavapipe, NVIDIA) keep the strict floor.
fn close_floor(device: &crate::RenderDevice, strict: f64) -> f64 {
    if device.capabilities.backend == wgpu::Backend::Metal {
        strict.min(0.97)
    } else {
        strict
    }
}

/// (largest channel difference, fraction of pixels whose channels all differ by ≤ 2).
fn diff(a: &[u8], b: &[u8]) -> (u8, f64) {
    let mut worst = 0u8;
    let mut close = 0usize;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        worst = worst.max(d);
        close += usize::from(d <= 2);
    }
    (worst, close as f64 / (a.len() / 4) as f64)
}

/// Fraction of `reference` pixels with no clipped channel (every channel < 250).
///
/// A pixel with a clipped channel has lost shading information in that channel (a saturated
/// sky blue or sunlit orange as much as a blown-out white), so a comparison that only reads such
/// pixels would report "identical" even when the other frame's shading differs. The
/// vis-buffer-vs-forward tests require at least [`MIN_UNCLIPPED_FRACTION`] of the reference to
/// be unclipped, so they keep failing loudly if the fixture's exposure ever lets the frames
/// blow out again.
const MIN_UNCLIPPED_FRACTION: f64 = 0.80;

fn unclipped_fraction(reference: &[u8]) -> f64 {
    let n = reference.len() / 4;
    let clipped = reference
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] >= 250 || p[1] >= 250 || p[2] >= 250)
        .count();
    (n - clipped) as f64 / n as f64
}

#[test]
fn visibility_frames_match_forward_frames() {
    let device = crate::test_device();
    let f = fixture(6);
    let settings = post();
    // Both renderers draw two frames and the second ones are compared: a fresh renderer's
    // frames keep changing for a few dozen frames on their own (the forward frame 0 vs 1 alone
    // differs by up to 107 levels), so a forward frame 0 against a vis-buffer frame 1 measures
    // that warm-up, not the vis-buffer path.
    let mut forward = comparison_renderer(&device);
    let fs = scene(&f, &settings, false, false);
    render(&mut forward, &device, &fs);
    let reference = render(&mut forward, &device, &fs);
    let mut vis = comparison_renderer(&device);
    vis.set_meshlet_builder(Some(single_level_builder()));
    let s = scene(&f, &settings, true, false);
    render(&mut vis, &device, &s);
    let pixels = render(&mut vis, &device, &s);
    let stats = vis.visbuffer_stats();
    assert_eq!(stats.frames, 2);
    assert_eq!(
        stats.instances as usize,
        f.models.len() + 1,
        "rocks and the ground take the path"
    );
    assert_eq!(stats.resident_meshes, 2);
    assert_eq!(stats.materials, 2);
    let counters = vis
        .visbuffer_counters(&device)
        .expect("readback")
        .expect("counters");
    assert!(
        counters.phase1_clusters > 0,
        "the second frame redraws last frame's clusters: {counters:?}"
    );
    assert_eq!(counters.dropped_clusters, 0);
    let (worst, close) = diff(&reference, &pixels);
    let unclipped = unclipped_fraction(&reference);
    assert!(
        close > close_floor(&device, 0.995),
        "{:.2}% of pixels within 2 levels (worst {worst})",
        close * 100.0
    );
    assert!(
        unclipped >= MIN_UNCLIPPED_FRACTION,
        "only {:.2}% of the forward reference is unclipped; the fixture is blown out \
         and the comparison is meaningless",
        unclipped * 100.0
    );
}

/// ADR-5807: DX12 clears the visibility IDs through `f32` values, which cannot hold the empty ID
/// (`u32::MAX`), so there phase 1 writes it with a full-screen draw. Forced here on any backend:
/// the filled frame is the cleared frame, byte for byte, background included (an ID left at the
/// driver's conversion shades the background as some cluster's triangle 0, which the N150 did).
#[test]
fn filling_the_empty_id_by_a_draw_gives_the_cleared_frame() {
    let device = crate::test_device();
    let f = fixture(3);
    let settings = post();
    let s = scene(&f, &settings, true, false);
    let frames = |fill: bool| {
        let mut renderer = comparison_renderer(&device);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        renderer.visbuffer_pass_mut().force_id_fill = fill;
        render(&mut renderer, &device, &s);
        let pixels = render(&mut renderer, &device, &s);
        let filled = renderer.visbuffer_pass_mut().fills_ids_by_draw();
        assert_eq!(renderer.visbuffer_stats().frames, 2);
        (pixels, filled)
    };
    let (cleared, by_draw) = frames(false);
    assert_eq!(
        by_draw,
        !crate::backend_policy::clears_integer_targets_exactly(device.capabilities.backend),
        "the draw only where the clear cannot hold the empty ID"
    );
    let (filled, by_draw) = frames(true);
    assert!(by_draw, "the forced frame filled the IDs by the draw");
    assert_eq!(
        diff(&cleared, &filled).0,
        0,
        "the filled frame is the cleared frame"
    );
}

/// Review w11 #2: a rigid hair card whose host left the material `ALPHA_OPAQUE` (no
/// `apply_alpha_mode`) still draws in its strategy's mode, so it stays out of the visibility
/// buffer (which has no hair shading and no alpha test). Only the ground takes the path.
#[test]
fn hair_cards_left_opaque_stay_out_of_the_visibility_buffer() {
    let device = crate::test_device();
    let mut f = fixture(2);
    for alpha in [crate::HairAlpha::Hashed, crate::HairAlpha::Sorted] {
        f.rock.hair = crate::HairFactors {
            alpha,
            ..crate::HairFactors::strands()
        };
        assert_eq!(f.rock.alpha_mode, crate::ALPHA_OPAQUE);
        let settings = post();
        let mut vis = comparison_renderer(&device);
        vis.set_meshlet_builder(Some(single_level_builder()));
        let s = scene(&f, &settings, true, false);
        render(&mut vis, &device, &s);
        let stats = vis.visbuffer_stats();
        assert_eq!(stats.frames, 1, "{alpha:?}: the ground keeps the path running");
        assert_eq!(
            stats.instances, 1,
            "{alpha:?}: only the ground takes the path, the hair cards draw forward"
        );
    }
}

#[test]
fn off_by_default_and_without_meshlets() {
    let device = crate::test_device();
    let f = fixture(2);
    let settings = post();
    // A builder alone does nothing while the profile switch is off.
    let mut renderer = FrameRenderer::new();
    renderer.set_meshlet_builder(Some(single_level_builder()));
    render(&mut renderer, &device, &scene(&f, &settings, false, false));
    assert_eq!(renderer.visbuffer_stats().frames, 0);
    // The switch alone does nothing without meshlets: every mesh stays forward.
    let mut bare = FrameRenderer::new();
    render(&mut bare, &device, &scene(&f, &settings, true, false));
    assert_eq!(bare.visbuffer_stats().frames, 0);
    // A builder that declines keeps the forward picture.
    let mut declined = FrameRenderer::new();
    declined.set_meshlet_builder(Some(Arc::new(|_: &[LabVertex], _: &[u32]| None)));
    render(&mut declined, &device, &scene(&f, &settings, true, false));
    assert_eq!(declined.visbuffer_stats().frames, 0);
}

#[test]
fn hidden_clusters_are_occlusion_culled_without_changing_the_frame() {
    let device = crate::test_device();
    let f = fixture(8);
    let settings = post();
    let s = scene(&f, &settings, true, true);
    let drawn = |occlusion: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_meshlet_builder(Some(single_level_builder()));
        renderer.set_visbuffer_settings(VisBufferSettings {
            occlusion,
            ..VisBufferSettings::default()
        });
        // Frame 1 draws everything in phase 2 (no history); frame 2 redraws it in phase 1 and
        // clears the bits of what the full depth hides; frame 3 draws only the visible clusters.
        render(&mut renderer, &device, &s);
        render(&mut renderer, &device, &s);
        let pixels = render(&mut renderer, &device, &s);
        let c = renderer
            .visbuffer_counters(&device)
            .expect("readback")
            .expect("counters");
        (c.phase1_clusters + c.phase2_clusters, c, pixels)
    };
    let (all, _, open_pixels) = drawn(false);
    let (culled, counters, culled_pixels) = drawn(true);
    assert!(
        culled * 10 < all * 9,
        "the wall hides clusters: {culled} drawn with occlusion vs {all} without ({counters:?})"
    );
    assert_eq!(
        counters.phase2_clusters, 0,
        "a static view finds nothing new in phase 2"
    );
    let (worst, close) = diff(&open_pixels, &culled_pixels);
    assert!(close > 0.999, "occlusion changed the frame (worst {worst})");
}

#[test]
fn warm_visibility_frames_create_no_pipelines_or_bind_groups() {
    let device = crate::test_device();
    let f = fixture(3);
    let settings = post();
    let s = scene(&f, &settings, true, false);
    let mut renderer = FrameRenderer::new();
    renderer.set_meshlet_builder(Some(single_level_builder()));
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vis frame path"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let target = crate::FrameTarget::full(&view, format, SIZE, SIZE);
    for _ in 0..24 {
        renderer.render_into(&device, &s, target).expect("warm-up");
    }
    let pipelines = crate::frame_stats::pipeline_creates();
    let groups = crate::frame_stats::bind_group_creates();
    for _ in 0..4 {
        renderer.render_into(&device, &s, target).expect("frame");
    }
    assert_eq!(crate::frame_stats::pipeline_creates(), pipelines);
    assert_eq!(
        crate::frame_stats::bind_group_creates(),
        groups,
        "{:?}",
        crate::frame_stats::bind_group_labels_from(groups)
    );
    assert!(renderer.visbuffer_stats().frames >= 28);
}

#[test]
fn resolve_module_composes_with_and_without_srgb_encode() {
    for encode in [false, true] {
        let mut defs = std::collections::HashMap::new();
        defs.insert(
            "VISBUFFER".to_string(),
            naga_oil::compose::ShaderDefValue::Bool(true),
        );
        if !encode {
            defs.insert(
                "skip_srgb_encode".to_string(),
                naga_oil::compose::ShaderDefValue::Bool(true),
            );
        }
        let module =
            crate::shader_lib::compose_source(super::gpu::resolve_source(), "vis_resolve.wgsl", defs)
                .expect("compose");
        let names: Vec<&str> = module.entry_points.iter().map(|e| e.name.as_str()).collect();
        for entry in ["vs_vis_fullscreen", "fs_vis_material_depth", "fs_vis_resolve"] {
            assert!(names.contains(&entry), "{entry} missing from {names:?}");
        }
        assert!(
            !names.contains(&"fs_main"),
            "the forward entry is replaced by shade_surface"
        );
    }
}

#[test]
fn moving_frames_with_temporal_aa_match_forward() {
    let device = crate::test_device();
    let f = fixture(5);
    let settings = PostSettings {
        taa: true,
        auto_exposure: false,
        bloom: false,
        exposure_ev: EXPOSURE_EV,
        ..PostSettings::default()
    };
    let run = |vis: bool| {
        let mut renderer = comparison_renderer(&device);
        if vis {
            renderer.set_meshlet_builder(Some(single_level_builder()));
        }
        let mut pixels = Vec::new();
        for i in 0..6 {
            let mut s = scene(&f, &settings, vis, false);
            s.quality.taau = true;
            s.eye[0] = -0.6 + 0.2 * i as f32;
            s.target[0] = s.eye[0];
            pixels = render(&mut renderer, &device, &s);
        }
        (pixels, renderer.visbuffer_stats().frames)
    };
    let (reference, none) = run(false);
    let (moving, frames) = run(true);
    assert_eq!(none, 0);
    assert_eq!(frames, 6);
    let (worst, close) = diff(&reference, &moving);
    let unclipped = unclipped_fraction(&reference);
    assert!(
        close > 0.99,
        "{:.2}% of pixels within 2 levels after a pan (worst {worst})",
        close * 100.0
    );
    assert!(
        unclipped >= MIN_UNCLIPPED_FRACTION,
        "only {:.2}% of the forward reference is unclipped; the fixture is blown out \
         and the comparison is meaningless",
        unclipped * 100.0
    );
}

/// Two levels over the same vertices, for counting which level a view selects (not a
/// watertight cut): the mesh's own clusters (parent error `error`) and the first half of its
/// triangles as coarse root clusters (own error `error`), so the coarse level has fewer clusters.
fn two_level_builder(error: f32) -> MeshletBuilder {
    Arc::new(move |vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let mut mesh = MeshletMesh::single_level(&positions, indices);
        let half: Vec<u32> = indices[..indices.len() / 6 * 3].to_vec();
        let coarse = MeshletMesh::single_level(&positions, &half);
        let whole = mesh.bounds;
        for m in &mut mesh.meshlets {
            m.parent = whole;
            m.parent_error = error;
        }
        let (v0, t0) = (mesh.vertices.len() as u32, mesh.triangles.len() as u32);
        for m in &coarse.meshlets {
            mesh.meshlets.push(super::Meshlet {
                lod: whole,
                lod_error: error,
                parent: whole,
                parent_error: super::ROOT_ERROR,
                vertex_offset: m.vertex_offset + v0,
                triangle_offset: m.triangle_offset + t0,
                level: 1,
                ..*m
            });
        }
        mesh.vertices.extend_from_slice(&coarse.vertices);
        mesh.triangles.extend_from_slice(&coarse.triangles);
        Some(mesh)
    })
}

#[test]
fn cluster_shadows_match_forward_shadows() {
    let device = crate::test_device();
    let f = fixture(5);
    let settings = post();
    // A tall pillar left of the view (45° off the axis, outside the 28.6° half-angle): off
    // screen, its shadow (the sun travels +x, −z) falls across the visible ground.
    let pillar = crate::box_mesh([1.0, 6.0, 1.0]);
    let with_pillar = |vis: bool| {
        let mut s = scene(&f, &settings, vis, false);
        s.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: 4,
            material_key: 1004,
            object_key: 4,
            vertices: &pillar.0,
            indices: &pillar.1,
            material: &f.floor,
            model: translate(-4.0, 6.0, 0.0, 1.0),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
        s
    };
    // The second forward frame, like the second vis-buffer frames below (see
    // `visibility_frames_match_forward_frames`).
    let mut forward = comparison_renderer(&device);
    render(&mut forward, &device, &with_pillar(false));
    let reference = render(&mut forward, &device, &with_pillar(false));
    let draw = |cluster_shadows: bool| {
        let mut renderer = comparison_renderer(&device);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        renderer.set_visbuffer_settings(VisBufferSettings {
            cluster_shadows,
            ..VisBufferSettings::default()
        });
        let s = with_pillar(true);
        render(&mut renderer, &device, &s);
        let pixels = render(&mut renderer, &device, &s);
        let counters = renderer.visbuffer_shadow_counters(&device).expect("readback");
        (pixels, counters)
    };
    let (forward_shadows, none) = draw(false);
    let (clusters, counters) = draw(true);
    assert!(none.is_none(), "no shadow cull without cluster shadows");
    let [work, drawn, dropped, dropped_work] = counters.expect("shadow counters");
    assert!(
        work > 0 && drawn > 0,
        "casters reached the shadow maps: {work} items, {drawn} clusters"
    );
    assert_eq!((dropped, dropped_work), (0, 0));
    let (worst, close) = diff(&forward_shadows, &clusters);
    assert!(
        close > 0.999,
        "cluster shadows vs forward shadows: {:.2}% within 2 levels (worst {worst})",
        close * 100.0
    );
    let (worst, close) = diff(&reference, &clusters);
    let unclipped = unclipped_fraction(&reference);
    assert!(
        close > close_floor(&device, 0.995),
        "vs the forward frame: {:.2}% within 2 levels (worst {worst})",
        close * 100.0
    );
    assert!(
        unclipped >= MIN_UNCLIPPED_FRACTION,
        "only {:.2}% of the forward reference is unclipped; the fixture is blown out \
         and the comparison is meaningless",
        unclipped * 100.0
    );
    // The off-screen pillar's shadow: the pixels it darkens in the forward frame are darkened
    // the same way by the cluster shadows.
    let mut bare = comparison_renderer(&device);
    render(&mut bare, &device, &scene(&f, &settings, false, false));
    let no_pillar = render(&mut bare, &device, &scene(&f, &settings, false, false));
    fn px(v: &[u8], i: usize) -> &[u8] {
        &v[i * 4..i * 4 + 3]
    }
    let differs = |a: &[u8], b: &[u8]| a.iter().zip(b).any(|(x, y)| x.abs_diff(*y) > 8);
    let shadowed: Vec<usize> = (0..no_pillar.len() / 4)
        .filter(|&i| differs(px(&no_pillar, i), px(&reference, i)))
        .collect();
    assert!(
        shadowed.len() > 20,
        "the pillar casts into the view: {} pixels",
        shadowed.len()
    );
    let matching = shadowed
        .iter()
        .filter(|&&i| !differs(px(&reference, i), px(&clusters, i)))
        .count();
    assert!(
        matching * 10 >= shadowed.len() * 9,
        "cluster shadows reproduce the off-screen caster's shadow on {matching} of {} pixels",
        shadowed.len()
    );
}

#[test]
fn shadow_lod_follows_the_shadow_maps_texel_size() {
    let device = crate::test_device();
    let f = fixture(3);
    let settings = post();
    let s = scene(&f, &settings, true, false);
    let shadow_clusters = |error: f32| {
        let mut renderer = FrameRenderer::new();
        renderer.set_meshlet_builder(Some(two_level_builder(error)));
        render(&mut renderer, &device, &s);
        renderer
            .visbuffer_shadow_counters(&device)
            .expect("readback")
            .expect("counters")[1]
    };
    // An error far below a texel picks the coarse level everywhere; one far above, the fine one.
    let coarse = shadow_clusters(1e-6);
    let fine = shadow_clusters(100.0);
    assert!(
        coarse > 0 && fine > coarse,
        "coarse {coarse} vs fine {fine} shadow clusters"
    );
}

#[test]
fn instance_sets_draw_like_the_mesh_list() {
    let device = crate::test_device();
    let f = fixture(4);
    let settings = post();
    let listed = scene(&f, &settings, true, false);
    let mut by_list = FrameRenderer::new();
    by_list.set_meshlet_builder(Some(single_level_builder()));
    render(&mut by_list, &device, &listed);
    let reference = render(&mut by_list, &device, &listed);

    // The same rocks as one persistent set; the frame's list keeps only the ground.
    let mut ground_only = scene(&f, &settings, true, false);
    ground_only.meshes.truncate(1);
    let mut by_set = FrameRenderer::new();
    by_set.set_meshlet_builder(Some(single_level_builder()));
    let set = super::VisInstanceSet {
        mesh_key: 2,
        vertices: &f.sphere.0,
        indices: &f.sphere.1,
        material_key: 1002,
        material: &f.rock,
        models: &f.models,
        shadow: ShadowCast::All,
        instance_shadows: &[],
    };
    assert!(by_set.set_instance_set(7, &set));
    render(&mut by_set, &device, &ground_only);
    let pixels = render(&mut by_set, &device, &ground_only);
    assert_eq!(
        by_set.visbuffer_stats().instances,
        by_list.visbuffer_stats().instances
    );
    let (worst, close) = diff(&reference, &pixels);
    assert!(
        close > 0.999,
        "set vs mesh list: {:.2}% within 2 levels (worst {worst})",
        close * 100.0
    );

    // Removing the set clears its pixels and its cached shadows (paged cascades). The reference
    // renders as many frames as `by_set` will have: a held view keeps refining its first frames.
    let mut plain = FrameRenderer::new();
    plain.set_meshlet_builder(Some(single_level_builder()));
    for _ in 0..3 {
        render(&mut plain, &device, &ground_only);
    }
    let empty = render(&mut plain, &device, &ground_only);
    assert!(by_set.remove_instance_set(7));
    render(&mut by_set, &device, &ground_only);
    let removed = render(&mut by_set, &device, &ground_only);
    let (worst, close) = diff(&empty, &removed);
    assert!(
        close > 0.999,
        "after removal: {:.2}% within 2 levels (worst {worst})",
        close * 100.0
    );
}

/// A quad in the XY plane whose front face points −Z (away from the fixture's camera and
/// sun): the camera sees its back face, lit by the sun through the flipped normal.
fn fin_facing_away() -> (Vec<LabVertex>, Vec<u32>) {
    let corners = [
        [-0.5, -0.5, 0.0],
        [0.5, -0.5, 0.0],
        [0.5, 0.5, 0.0],
        [-0.5, 0.5, 0.0],
    ];
    let vertices = corners
        .iter()
        .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, -1.0], [p[0] + 0.5, 0.5 - p[1]]))
        .collect();
    (vertices, vec![0, 2, 1, 0, 3, 2])
}

/// Levels a fin pixel may differ between the cluster and forward frames and still count as
/// drawn alike. The sunlit fins sit near 150 levels, where the ambient terms of the two paths
/// (resolve versus forward) differ by a few levels; ADR-1447's brighter sky moved three pixels
/// from 2 to 3-5 levels (ADR-1459).
const BACK_FACE_TOLERANCE: u8 = 6;

#[test]
fn double_sided_instances_draw_back_faces_and_cast_like_forward() {
    let device = crate::test_device();
    let f = fixture(2);
    // Exposed for the 100 klx sun: at the fixture's EV 0 the frame is white and the fins vanish.
    let settings = PostSettings {
        exposure_ev: 15.0,
        ..post()
    };
    let fin = fin_facing_away();
    // Untextured: a minified checker differs between the resolve's analytic gradients and the
    // forward pass's derivatives (as on the spheres), which is not what this test is about.
    let materials = [false, true].map(|double_sided| LabMaterial {
        base_color: [0.55, 0.35, 0.25, 1.0],
        double_sided,
        ..LabMaterial::default()
    });
    let with_fins = |vis: bool, double_sided: bool| {
        let mut s = scene(&f, &settings, vis, false);
        // Balanced: under Depth the probes keep converging between the two renders, which is
        // not what this compares (the same pin as the mesh_occluders render test).
        s.quality.gi_probes = false;
        for i in 0..6 {
            s.meshes.push(FrameMesh {
                is_static: false,
                mesh_key: 5,
                material_key: 1005 + u64::from(double_sided),
                object_key: 500 + i,
                vertices: &fin.0,
                indices: &fin.1,
                material: &materials[usize::from(double_sided)],
                model: translate(-2.0 + 0.8 * i as f32, 0.6, -1.0 - 0.7 * i as f32, 1.2),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
        s
    };
    let mut forward = FrameRenderer::new();
    let reference = render(&mut forward, &device, &with_fins(false, true));
    let draw = |double_sided: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_meshlet_builder(Some(single_level_builder()));
        renderer.set_visbuffer_settings(VisBufferSettings {
            cluster_shadows: true,
            ..VisBufferSettings::default()
        });
        let s = with_fins(true, double_sided);
        render(&mut renderer, &device, &s);
        let pixels = render(&mut renderer, &device, &s);
        (pixels, renderer.visbuffer_stats(), renderer.draw_stats())
    };
    let (culled, _, _) = draw(false);
    let (pixels, stats, draws) = draw(true);
    // The fins are visibility-buffer instances, not forward draws.
    assert_eq!(stats.instances as usize, f.models.len() + 1 + 6, "{stats:?}");
    assert_eq!(draws.double_sided_per_object, 0, "{draws:?}");
    // Against the forward frame about as closely as the single-sided pair: this fixture's vis
    // and forward frames differ at the far ground edge and on one-pixel silhouettes (the two
    // rasters place an edge a little differently), and six fins add silhouette length.
    let mut forward_ss = FrameRenderer::new();
    let reference_ss = render(&mut forward_ss, &device, &with_fins(false, false));
    let (_, baseline) = diff(&reference_ss, &culled);
    let (worst, close) = diff(&reference, &pixels);
    assert!(
        close >= baseline - 0.01,
        "double-sided clusters vs forward double-sided draws: {:.2}% within 2 levels (worst {worst}), \
         single-sided pair {:.2}%",
        close * 100.0,
        baseline * 100.0
    );
    // The fins' pixels (where the forward frame changes with double-sided fins): the clusters
    // draw the same back faces.
    let px = |v: &[u8], i: usize| -> [u8; 3] { [v[i * 4], v[i * 4 + 1], v[i * 4 + 2]] };
    let differs =
        |a: [u8; 3], b: [u8; 3], tolerance: u8| a.iter().zip(b).any(|(x, y)| x.abs_diff(y) > tolerance);
    let is_fin = |i: usize| differs(px(&reference, i), px(&reference_ss, i), 8);
    // Interior pixels only (all four neighbours are fin too): silhouettes differ as above.
    let n = SIZE as usize;
    let fin_px: Vec<usize> = (0..reference.len() / 4)
        .filter(|&i| {
            let (x, y) = (i % n, i / n);
            x > 0
                && y > 0
                && x + 1 < n
                && y + 1 < n
                && [i, i - 1, i + 1, i - n, i + n].into_iter().all(is_fin)
        })
        .collect();
    assert!(
        fin_px.len() > 60,
        "the fins cover the view: {} pixels",
        fin_px.len()
    );
    let worst_fin = fin_px
        .iter()
        .map(|&i| {
            px(&reference, i)
                .iter()
                .zip(px(&pixels, i))
                .map(|(a, b)| a.abs_diff(b))
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0);
    let matching = fin_px
        .iter()
        .filter(|&&i| !differs(px(&reference, i), px(&pixels, i), BACK_FACE_TOLERANCE))
        .count();
    // A culled back face would match none of them (it shows the sky, 60+ levels away). On
    // lavapipe 209 of 214 match; the rest sit where the cluster and forward shadow / AO terms
    // differ (up to 31 levels, as the single-sided paths do in
    // `cluster_shadows_match_forward_shadows`).
    assert!(
        matching * 100 >= fin_px.len() * 95,
        "back faces: {matching} of {} fin pixels match the forward frame (worst {worst_fin})",
        fin_px.len()
    );
    // Single-sided fins vanish from behind, in the clusters as in the forward frame.
    let gone = fin_px
        .iter()
        .filter(|&&i| !differs(px(&reference_ss, i), px(&culled, i), 8))
        .count();
    assert!(
        gone * 100 >= fin_px.len() * 98,
        "single-sided: {gone} of {} pixels",
        fin_px.len()
    );
}

/// `remove_meshlets` / `has_meshlets` on cooked meshlets that have not drawn yet (no device).
#[test]
fn removing_meshlets_forgets_them_once() {
    let f = fixture(1);
    let positions: Vec<[f32; 3]> = f.sphere.0.iter().map(|v| v.position).collect();
    let mut renderer = FrameRenderer::new();
    assert!(!renderer.has_meshlets(9));
    assert!(!renderer.remove_meshlets(9), "nothing to remove");
    renderer.insert_meshlets(9, Arc::new(MeshletMesh::single_level(&positions, &f.sphere.1)));
    assert!(renderer.has_meshlets(9));
    assert_eq!(renderer.visbuffer_arena_stats().pending, 1);
    assert!(renderer.remove_meshlets(9));
    assert!(!renderer.has_meshlets(9));
    assert!(!renderer.remove_meshlets(9), "a second remove finds nothing");
    let stats = renderer.visbuffer_arena_stats();
    assert_eq!((stats.pending, stats.meshes, stats.buffer_creates), (0, 0, 0));
}

/// Streaming churn through the frame: every frame four meshes with cooked meshlets arrive
/// (`insert_meshlets`) and four leave (`remove_meshlets`), sixteen drawing at a time. After the
/// first frame the cluster arena neither grows nor creates a buffer, every resident mesh draws
/// through the visibility buffer, and the arena holds exactly the drawn meshes.
#[test]
fn streaming_meshlets_create_no_arena_buffers_once_warm() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let positions: Vec<[f32; 3]> = f.sphere.0.iter().map(|v| v.position).collect();
    let meshlets = Arc::new(MeshletMesh::single_level(&positions, &f.sphere.1));
    let mut vis = comparison_renderer(&device);
    const LIVE: u64 = 16;
    const STEP: u64 = 4;
    let mut warm = None;
    for frame in 0..12u64 {
        let first = 100 + frame * STEP;
        let keys: Vec<u64> = (first..first + LIVE).collect();
        // Arrivals: every key on the first frame, then the newest four.
        let arriving = if frame == 0 {
            &keys[..]
        } else {
            &keys[(LIVE - STEP) as usize..]
        };
        for &key in arriving {
            vis.insert_meshlets(key, Arc::clone(&meshlets));
        }
        let mut s = scene(&f, &settings, true, false);
        s.meshes.truncate(1);
        for (i, &key) in keys.iter().enumerate() {
            let x = (i % 4) as f32 * 1.2 - 1.8;
            let z = -((i / 4) as f32) * 1.2;
            s.meshes.push(FrameMesh {
                is_static: false,
                mesh_key: key,
                material_key: 2000,
                object_key: 10_000 + key,
                vertices: &f.sphere.0,
                indices: &f.sphere.1,
                material: &f.rock,
                model: translate(x, 0.4, z, 0.4),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
        render(&mut vis, &device, &s);
        assert_eq!(
            vis.visbuffer_stats().instances as u64,
            LIVE,
            "frame {frame}: every resident mesh draws through the visibility buffer"
        );
        let arena = vis.visbuffer_arena_stats();
        assert_eq!(
            u64::from(arena.meshes),
            LIVE,
            "frame {frame}: the arena holds the drawn meshes"
        );
        assert_eq!(arena.pending, 0);
        match warm {
            None => warm = Some((arena.buffer_creates, arena.growths)),
            Some(at_warm) => assert_eq!(
                (arena.buffer_creates, arena.growths),
                at_warm,
                "frame {frame}: the arena grew or created a buffer under churn"
            ),
        }
        // Departures: the oldest four.
        for &key in &keys[..STEP as usize] {
            assert!(vis.remove_meshlets(key), "frame {frame}: key {key} was resident");
        }
    }
    let arena = vis.visbuffer_arena_stats();
    assert!(arena.buffer_creates >= 2, "both streams have a buffer");
    assert!(!arena.capped);
}

/// The whole path, not just the cluster arena, creates nothing once warm (ADR-355): with
/// cluster shadows on, a persistent instance set and the mesh list streaming four meshes in and
/// four out every frame, no frame after the warm-up creates a pipeline, bind group, buffer or
/// texture through the path, while it keeps uploading (instances, arena ranges). The first
/// frame's creates are all seen: the fourteen buffers of the path's first `prepare` (the two
/// arena streams and twelve lists and parameter blocks that once bypassed the tracked device),
/// its three targets and its pipelines.
#[test]
fn visbuffer_path_creates_nothing_once_warm() {
    let device = crate::test_device();
    let f = fixture(3);
    let settings = post();
    let positions: Vec<[f32; 3]> = f.sphere.0.iter().map(|v| v.position).collect();
    let meshlets = Arc::new(MeshletMesh::single_level(&positions, &f.sphere.1));
    let mut vis = comparison_renderer(&device);
    vis.set_visbuffer_settings(VisBufferSettings {
        cluster_shadows: true,
        ..VisBufferSettings::default()
    });
    assert_eq!(
        vis.visbuffer_create_stats(),
        super::VisBufferCreateStats::default()
    );
    // The persistent set: the fixture's rocks, under their own mesh key.
    const SET_MESH: u64 = 50;
    vis.insert_meshlets(SET_MESH, Arc::clone(&meshlets));
    let set = super::VisInstanceSet {
        mesh_key: SET_MESH,
        vertices: &f.sphere.0,
        indices: &f.sphere.1,
        material_key: 1050,
        material: &f.rock,
        models: &f.models,
        shadow: ShadowCast::All,
        instance_shadows: &[],
    };
    assert!(vis.set_instance_set(7, &set));
    const LIVE: u64 = 16;
    const STEP: u64 = 4;
    const WARM: u64 = 4;
    const FRAMES: u64 = 16;
    let mut warm_total = None;
    for frame in 0..FRAMES {
        let first = 100 + frame * STEP;
        let keys: Vec<u64> = (first..first + LIVE).collect();
        let arriving = if frame == 0 {
            &keys[..]
        } else {
            &keys[(LIVE - STEP) as usize..]
        };
        for &key in arriving {
            vis.insert_meshlets(key, Arc::clone(&meshlets));
        }
        let mut s = scene(&f, &settings, true, false);
        s.meshes.truncate(1);
        for (i, &key) in keys.iter().enumerate() {
            let x = (i % 4) as f32 * 1.2 - 1.8;
            let z = 1.0 - (i / 4) as f32 * 1.2;
            s.meshes.push(FrameMesh {
                is_static: false,
                mesh_key: key,
                material_key: 2000,
                object_key: 10_000 + key,
                vertices: &f.sphere.0,
                indices: &f.sphere.1,
                material: &f.rock,
                model: translate(x, 0.4, z, 0.4),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
        render(&mut vis, &device, &s);
        assert_eq!(
            vis.visbuffer_stats().instances as u64,
            LIVE + f.models.len() as u64,
            "frame {frame}: the set and every streamed mesh draw through the visibility buffer"
        );
        let creates = vis.visbuffer_create_stats();
        assert!(
            creates.frames > frame,
            "one frame start per render at least: {creates:?}"
        );
        assert!(
            creates.last_frame.upload_bytes > 0,
            "frame {frame}: the path ran (it uploads its parameters every frame)"
        );
        if frame == 0 {
            let warmup = creates.last_frame;
            assert!(
                warmup.buffers >= 14,
                "the first prepare's buffers are counted: {warmup:?}"
            );
            assert!(warmup.textures >= 3, "ids, material depth and Hi-Z: {warmup:?}");
            assert!(warmup.bind_groups > 0 && warmup.pipelines > 0, "{warmup:?}");
        }
        if frame + 1 == WARM {
            warm_total = Some(creates.total);
        } else if frame >= WARM {
            assert_eq!(
                creates.last_frame.objects(),
                0,
                "frame {frame} created GPU objects in steady state: {:?}",
                creates.last_frame
            );
            assert!(creates.quiet_frames > frame - WARM, "frame {frame}: {creates:?}");
        }
        for &key in &keys[..STEP as usize] {
            assert!(vis.remove_meshlets(key), "frame {frame}: key {key} was resident");
        }
    }
    let end = vis.visbuffer_create_stats();
    let warm = warm_total.expect("the warm-up ran");
    assert_eq!(
        (
            end.total.buffers,
            end.total.bind_groups,
            end.total.textures,
            end.total.pipelines
        ),
        (warm.buffers, warm.bind_groups, warm.textures, warm.pipelines),
        "no create after the warm-up"
    );
    assert!(end.total.upload_bytes > warm.upload_bytes);
    // The arena's own counter agrees: it has been flat since the warm-up as well.
    let arena = vis.visbuffer_arena_stats();
    assert!(arena.buffer_creates >= 2 && arena.buffer_creates <= end.total.buffers);
}

/// The third frame of `s` through a fresh renderer, with its cull counters. Frame 1 has no
/// history and draws everything in phase 2; frame 3 draws what stayed visible in phase 1.
fn software_frame(
    device: &crate::RenderDevice,
    s: &FrameScene<'_>,
    settings: VisBufferSettings,
) -> (Vec<u8>, super::VisBufferCounters) {
    let mut renderer = comparison_renderer(device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    renderer.set_visbuffer_settings(settings);
    render(&mut renderer, device, s);
    render(&mut renderer, device, s);
    let pixels = render(&mut renderer, device, s);
    let counters = renderer
        .visbuffer_counters(device)
        .expect("readback")
        .expect("counters");
    (pixels, counters)
}

fn software_on() -> VisBufferSettings {
    VisBufferSettings {
        software_raster: true,
        ..VisBufferSettings::default()
    }
}

/// The compute rasterizer follows the hardware's coverage and depth (ADR-1887): the frame with
/// the small clusters in compute matches the hardware frame, with and without a large hardware
/// occluder in front of them, and big clusters stay in hardware.
#[test]
fn software_raster_matches_the_hardware_raster() {
    let device = crate::test_device();
    let f = fixture(10);
    let settings = post();
    for wall in [false, true] {
        let s = scene(&f, &settings, true, wall);
        let (hardware, off) = software_frame(&device, &s, VisBufferSettings::default());
        // This lens stretches a sphere's image by at most 1.6, so 26 px keeps every cluster the
        // route took at 16 px before it counted the stretch.
        let wide_route = VisBufferSettings {
            software_raster_max_px: 26,
            ..software_on()
        };
        let (software, on) = software_frame(&device, &s, wide_route);
        assert_eq!(off.software_clusters, 0, "off by default: {off:?}");
        assert!(on.software_clusters >= 8, "wall {wall}: {on:?}");
        assert!(
            on.phase1_clusters + on.phase2_clusters > 0,
            "big clusters stay in hardware: {on:?}"
        );
        assert!(
            on.phase1_clusters + on.phase2_clusters < off.phase1_clusters + off.phase2_clusters,
            "the small ones left the hardware lists: {on:?} vs {off:?}"
        );
        assert_eq!(on.dropped_clusters, 0);
        let (worst, close) = diff(&hardware, &software);
        eprintln!("wall {wall}: worst {worst}, close {close}, {on:?}");
        let floor = close_floor(&device, 0.985);
        assert!(
            close >= floor,
            "wall {wall}: {close} of the pixels close (worst {worst})"
        );
    }
}

/// Two renderers draw the same bytes (the list order changes between runs, the depth-then-ID
/// passes do not depend on it).
#[test]
fn software_raster_is_deterministic() {
    let device = crate::test_device();
    let f = fixture(10);
    let settings = post();
    let s = scene(&f, &settings, true, false);
    let (first, a) = software_frame(&device, &s, software_on());
    let (second, b) = software_frame(&device, &s, software_on());
    assert_eq!(a.software_clusters, b.software_clusters);
    assert!(first == second, "the software frames differ");
}

/// The software range holds `max_clusters` clusters; once it is full the cull hands the rest to
/// the hardware lists, so nothing drops (ADR-1887 (8)). At the smallest capacity, 256 small rocks
/// fill the range and the frame still matches the hardware frame.
#[test]
fn software_raster_overflow_falls_back_to_the_hardware() {
    let device = crate::test_device();
    let f = fixture(16);
    let settings = post();
    let mut s = scene(&f, &settings, true, false);
    s.eye = [0.0, 6.0, 24.0];
    let (hardware, off) = software_frame(&device, &s, VisBufferSettings::default());
    let small = VisBufferSettings {
        max_clusters: 1024,
        ..software_on()
    };
    let (software, on) = software_frame(&device, &s, small);
    eprintln!("hardware {off:?}\nsoftware {on:?}");
    assert_eq!(on.software_clusters, 1024, "the software range fills: {on:?}");
    assert!(
        on.phase1_clusters + on.phase2_clusters >= 64,
        "the overflow draws in hardware: {on:?}"
    );
    assert_eq!(on.dropped_clusters, 0, "nothing drops: {on:?}");
    let (worst, close) = diff(&hardware, &software);
    assert!(
        close >= close_floor(&device, 0.985),
        "{close} of the pixels close (worst {worst})"
    );
}

/// A sphere's image stretches off the view axis, and `vis_software.wgsl` keeps its edge products
/// in an `i32` only if the route counts that. Under a wide lens the same rock, at the same depth,
/// goes to compute on the axis and to hardware in the corner.
#[test]
fn software_raster_counts_the_stretch_off_the_view_axis() {
    let device = crate::test_device();
    let settings = post();
    // Clear of the ground, at one depth: the corner is 63° off the axis, a stretch of 4.9.
    let on_axis = translate(0.0, 3.0, 0.0, 1.0);
    let corner = translate(14.0, 17.0, 0.0, 1.0);
    let frame = |models: Vec<[[f32; 4]; 4]>| {
        let f = Fixture { models, ..fixture(0) };
        let mut s = scene(&f, &settings, true, false);
        s.eye = [0.0, 3.0, 10.0];
        s.target = [0.0, 3.0, 0.0];
        s.fov_y = 2.0;
        software_frame(
            &device,
            &s,
            VisBufferSettings {
                software_raster_max_px: 6,
                ..software_on()
            },
        )
        .1
    };
    let alone = frame(vec![on_axis]);
    let both = frame(vec![on_axis, corner]);
    assert!(alone.software_clusters >= 2, "{alone:?}");
    assert_eq!(
        both.software_clusters, alone.software_clusters,
        "{alone:?} {both:?}"
    );
    assert!(
        both.phase1_clusters + both.phase2_clusters > alone.phase1_clusters + alone.phase2_clusters,
        "the corner rock draws in hardware: {alone:?} {both:?}"
    );
}

/// A cluster that reaches the near plane has to be clipped, so it stays in hardware: with the
/// near plane through the nearest rocks the frame still matches.
#[test]
fn software_raster_leaves_clusters_at_the_near_plane_to_the_hardware() {
    let device = crate::test_device();
    let f = fixture(10);
    let settings = post();
    let mut s = scene(&f, &settings, true, false);
    s.near = 5.0;
    let (hardware, _) = software_frame(&device, &s, VisBufferSettings::default());
    let (software, on) = software_frame(&device, &s, software_on());
    assert!(on.software_clusters >= 8, "{on:?}");
    let (worst, close) = diff(&hardware, &software);
    assert!(
        close >= close_floor(&device, 0.985),
        "{close} of the pixels close (worst {worst})"
    );
}

/// Under temporal AA the software clusters get the jittered projection and their motion
/// vectors from the shared ID target.
#[test]
fn software_raster_matches_under_temporal_aa() {
    let device = crate::test_device();
    let f = fixture(8);
    let settings = PostSettings { taa: true, ..post() };
    let frames = |software: VisBufferSettings| {
        let mut renderer = comparison_renderer(&device);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        renderer.set_visbuffer_settings(software);
        let mut last = Vec::new();
        for frame in 0..6 {
            let mut s = scene(&f, &settings, true, false);
            s.eye[0] = frame as f32 * 0.05;
            last = render(&mut renderer, &device, &s);
        }
        last
    };
    let (worst, close) = diff(&frames(VisBufferSettings::default()), &frames(software_on()));
    eprintln!("taa: worst {worst}, close {close}");
    assert!(
        close >= close_floor(&device, 0.97),
        "{close} of the pixels close (worst {worst})"
    );
}

/// The rasterizer's words, parameters and bind groups exist from its first frame on; a warm
/// frame creates nothing (ADR-355), and a size change makes new words.
#[test]
fn software_raster_creates_nothing_once_warm() {
    let device = crate::test_device();
    let f = fixture(4);
    let settings = post();
    let s = scene(&f, &settings, true, false);
    let mut renderer = comparison_renderer(&device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    renderer.set_visbuffer_settings(software_on());
    for _ in 0..3 {
        render(&mut renderer, &device, &s);
    }
    for frame in 0..4 {
        render(&mut renderer, &device, &s);
        let creates = renderer.visbuffer_create_stats().last_frame;
        assert_eq!(creates.objects(), 0, "warm frame {frame}: {creates:?}");
    }
    let mut smaller = scene(&f, &settings, true, false);
    smaller.width = SIZE / 2;
    smaller.height = SIZE / 2;
    render(&mut renderer, &device, &smaller);
    assert!(renderer.visbuffer_create_stats().last_frame.buffers >= 2);
}

/// The frame's GPU cost with the compute rasterizer on and off on a far field of rocks
/// (`--ignored`; prints, asserts nothing about time). On lavapipe both paths are CPU code, so
/// this is a correctness-path number only: the lead runs it on the 5080 and the 7900 at a real
/// size (`REGIA_SOFT_RASTER_SIZE=2560x1440`).
#[test]
#[ignore = "timing printout; run on the bench GPU"]
fn software_raster_cost() {
    let device = crate::test_device();
    let (width, height) = regia_core::brand::env_var("REGIA_SOFT_RASTER_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((SIZE, SIZE));
    let f = fixture(30);
    let settings = post();
    for on in [false, true, false, true] {
        let mut renderer = comparison_renderer(&device);
        renderer.set_gpu_timing(&device, true);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        renderer.set_visbuffer_settings(VisBufferSettings {
            software_raster: on,
            ..VisBufferSettings::default()
        });
        let mut s = scene(&f, &settings, true, false);
        s.width = width;
        s.height = height;
        s.eye = [0.0, 6.0, 24.0];
        for _ in 0..8 {
            render(&mut renderer, &device, &s);
        }
        let frames = 24;
        // `cull_ms` spans the cluster cull, both rasters and the merges, then the colour cull.
        let mut geometry = 0.0f64;
        for _ in 0..frames {
            render(&mut renderer, &device, &s);
            geometry += f64::from(renderer.gpu_timings().cull_ms);
        }
        let counters = renderer.visbuffer_counters(&device).expect("readback");
        eprintln!(
            "software_raster={on} {width}x{height}: geometry span {:.3} ms (mean of {frames}); {counters:?}",
            geometry / f64::from(frames),
        );
    }
}

/// The Hi-Z footprint `vis_cull.wgsl` (`hiz_occluded`) samples, in pixels from the projected
/// centre: focal length `f` (pixels), the sphere's projected centre `ndc`, view depth `w`,
/// radius `r`, screen `size`. The stretch is the shader's `1 + tan²θ`.
fn hiz_footprint_px(f: f32, ndc: [f32; 2], w: f32, r: f32, size: [f32; 2], stretch: bool) -> f32 {
    let off = [0.5 * size[0] * ndc[0] / f, 0.5 * size[1] * ndc[1] / f];
    let k = if stretch {
        1.0 + off[0] * off[0] + off[1] * off[1]
    } else {
        1.0
    };
    r * f * k / (w - r)
}

/// Audit v034 WGSL M1: the sphere's real image, measured by projecting points of its surface,
/// reaches farther from the projected centre than the on-axis footprint once it sits off the view
/// axis (45°: 224 px against 165 px at f = 1000 px), so `hiz_occluded` sampled too few texels and
/// could cull a cluster whose rim was visible. With the off-axis stretch the footprint covers the
/// image at every angle and distance a frame can hold.
#[test]
fn the_hiz_footprint_covers_the_sphere_off_the_view_axis() {
    let f = 1000.0f32;
    let size = [2400.0f32, 1600.0];
    let r = 1.0f32;
    let mut on_axis_short = false;
    for deg in [0.0f32, 15.0, 30.0, 45.0, 55.0] {
        for dist in [2.5f32, 4.0, 10.0, 40.0] {
            for (dx, dy) in [
                (1.0f32, 0.0f32),
                (0.0, 1.0),
                (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2),
            ] {
                let t = deg.to_radians().tan();
                // View space, +z forward: the direction (tx, ty, 1) at angle `deg` off the axis.
                let dir = [t * dx, t * dy, 1.0];
                let len = (dir[0] * dir[0] + dir[1] * dir[1] + 1.0).sqrt();
                let c = [dir[0] / len * dist, dir[1] / len * dist, dist / len];
                if c[2] - r <= 0.1 {
                    continue;
                }
                let project = |p: [f32; 3]| [f * p[0] / p[2], f * p[1] / p[2]];
                let centre = project(c);
                let ndc = [centre[0] / (0.5 * size[0]), centre[1] / (0.5 * size[1])];
                // The farthest projected surface point from the projected centre.
                let mut reach = 0.0f32;
                for i in 0..96 {
                    for j in 1..48 {
                        let (phi, theta) = (
                            i as f32 / 96.0 * std::f32::consts::TAU,
                            j as f32 / 48.0 * std::f32::consts::PI,
                        );
                        let p = [
                            c[0] + r * theta.sin() * phi.cos(),
                            c[1] + r * theta.sin() * phi.sin(),
                            c[2] + r * theta.cos(),
                        ];
                        let q = project(p);
                        reach = reach.max(((q[0] - centre[0]).powi(2) + (q[1] - centre[1]).powi(2)).sqrt());
                    }
                }
                let stretched = hiz_footprint_px(f, ndc, c[2], r, size, true);
                assert!(
                    stretched >= reach,
                    "{deg}° at {dist} m: footprint {stretched} px, image reaches {reach} px"
                );
                if deg == 45.0 && dist == 10.0 && hiz_footprint_px(f, ndc, c[2], r, size, false) < reach * 0.9
                {
                    on_axis_short = true;
                }
            }
        }
    }
    assert!(
        on_axis_short,
        "the on-axis footprint falls short at 45°: the case this guards"
    );
    // The shader applies the stretch the same way.
    let shader = include_str!("vis_cull.wgsl");
    let body = shader.split("fn hiz_occluded").nth(1).unwrap_or_default();
    assert!(
        body.contains("let px = radius * params.screen.w * stretch / (c.w - radius);"),
        "hiz_occluded's footprint lost its off-axis stretch"
    );
}

mod close;
mod cull_cost;
mod cull_order;
mod lod_hysteresis;
mod masked;
mod residency;
mod single_pass;
mod software_double_sided;
mod wind;
