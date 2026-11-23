// SPDX-License-Identifier: MPL-2.0
//! Swaying plants from the mesh list on the visibility path (ADR-3916), with the frame hookup, on a
//! texture-table device of whatever adapter `RenderDevice::headless` picks (lavapipe in CI).
//!
//! A row of trees (a solid trunk sheet and a leaf-card crown, both adopted with
//! `FoliageSway::adopt`) stands before a wall over a ground, under a shadowing sun. The frames at fixed
//! clock and wind values are compared with the forward path's; `visbuffer::tests::wind` has what a
//! persistent set can check without the mesh list.

use std::sync::Arc;

use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::visbuffer::MeshletBuilder;
use regia_render::visbuffer::sway::SwaySkip;
use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FogSettings, FoliageSway, FrameMesh, FrameQuality, FrameRenderer,
    FrameScene, LabMaterial, LabTexture, LabVertex, PostSettings, RenderDevice, RenderError, RenderProfile,
    RenderProfilePreset, ShadowCast,
};
use regia_test_utils::golden::ssim_rgba8;

const SIZE: u32 = 128;
const TREES: usize = 7;
const STRICT: f64 = 0.985;

static DEVICE_CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn table_device() -> Result<RenderDevice, RenderError> {
    let _guard = DEVICE_CREATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    regia_render::texture_table::set_device_level_request(regia_render::texture_table::TEXTURE_TABLE_LEVEL);
    let device = RenderDevice::headless();
    regia_render::texture_table::set_device_level_request(0);
    device
}

/// A device with the table, or `None` (the test then has nothing to check here).
fn device_or_skip() -> Option<RenderDevice> {
    let device = table_device().expect("device");
    if regia_render::texture_table::device_support(&device.device)
        .capacity()
        .is_none()
    {
        eprintln!("skip: the adapter cannot carry the texture table");
        return None;
    }
    Some(device)
}

fn cooked_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        build_meshlet_mesh_with_attributes(
            &positions,
            Some(MeshletAttributes {
                normals: &normals,
                uvs: &uvs,
            }),
            indices,
            &MeshletSettings::default(),
        )
        .ok()
    })
}

fn leaf_texture() -> LabTexture {
    let size = 64u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let u = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let v = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let a = if u * u / 0.55 + v * v / 0.95 < 1.0 { 255 } else { 0 };
            rgba.extend_from_slice(&[60 + (x % 7) as u8 * 10, 150, 50, a]);
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

/// `n` × `n` quads from `(x0, y0)` to `(x1, y1)` at depth `z`, facing +Z, appended to `out`.
fn add_sheet(out: &mut (Vec<LabVertex>, Vec<u32>), n: u32, [x0, y0]: [f32; 2], [x1, y1]: [f32; 2], z: f32) {
    let base = out.0.len() as u32;
    for j in 0..=n {
        for i in 0..=n {
            let (u, v) = (i as f32 / n as f32, j as f32 / n as f32);
            out.0.push(LabVertex::from_pos_normal_uv(
                [x0 + (x1 - x0) * u, y0 + (y1 - y0) * v, z],
                [0.0, 0.0, 1.0],
                [u, 1.0 - v],
            ));
        }
    }
    for j in 0..n {
        for i in 0..n {
            let at = |i: u32, j: u32| base + j * (n + 1) + i;
            out.1.extend_from_slice(&[
                at(i, j),
                at(i + 1, j),
                at(i + 1, j + 1),
                at(i, j),
                at(i + 1, j + 1),
                at(i, j + 1),
            ]);
        }
    }
}

/// A trunk: one tall sheet with its origin at the root.
fn trunk() -> (Vec<LabVertex>, Vec<u32>) {
    let mut out = (Vec::new(), Vec::new());
    add_sheet(&mut out, 12, [-0.08, 0.0], [0.08, 2.0], 0.0);
    out
}

/// A crown: leaf cards around the trunk's top, each 6 × 6 quads.
fn crown() -> (Vec<LabVertex>, Vec<u32>) {
    let mut out = (Vec::new(), Vec::new());
    for row in 0..4 {
        for column in 0..4 {
            let (x, y) = (column as f32 * 0.3 - 0.6, 1.1 + row as f32 * 0.25);
            let z = 0.05 + ((row + column) % 3) as f32 * 0.05;
            add_sheet(&mut out, 6, [x, y], [x + 0.28, y + 0.42], z);
        }
    }
    out
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

struct Fixture {
    trunk: (Vec<LabVertex>, Vec<u32>),
    crown: (Vec<LabVertex>, Vec<u32>),
    ground: (Vec<LabVertex>, Vec<u32>),
    wall: (Vec<LabVertex>, Vec<u32>),
    bark: LabMaterial,
    leaf: LabMaterial,
    floor: LabMaterial,
}

fn sway() -> FoliageSway {
    FoliageSway {
        stiffness: 0.05,
        height_m: 2.0,
        flutter_m: 0.02,
        gust_period_s: 4.0,
        max_wind_m_per_s: 20.0,
        ..FoliageSway::default()
    }
}

fn plain_bark() -> LabMaterial {
    LabMaterial {
        base_color: [0.4, 0.25, 0.15, 1.0],
        double_sided: true,
        ..LabMaterial::default()
    }
}

fn plain_leaf() -> LabMaterial {
    LabMaterial {
        albedo: Some(leaf_texture()),
        alpha_mode: ALPHA_MASK,
        alpha_cutoff: 0.5,
        double_sided: true,
        ..LabMaterial::default()
    }
}

fn fixture(adopt: bool) -> Fixture {
    let (bark, leaf) = (plain_bark(), plain_leaf());
    Fixture {
        trunk: trunk(),
        crown: crown(),
        ground: regia_render::box_mesh([20.0, 0.05, 20.0]),
        wall: regia_render::box_mesh([6.0, 3.0, 0.1]),
        bark: if adopt {
            sway().adopt(&bark).expect("adopt")
        } else {
            bark
        },
        leaf: if adopt {
            sway().adopt(&leaf).expect("adopt")
        } else {
            leaf
        },
        floor: LabMaterial::default(),
    }
}

fn post() -> PostSettings {
    PostSettings {
        taa: false,
        auto_exposure: false,
        bloom: false,
        exposure_ev: 15.0,
        ..PostSettings::default()
    }
}

fn mesh<'a>(
    key: u64,
    object: u64,
    data: &'a (Vec<LabVertex>, Vec<u32>),
    material: &'a LabMaterial,
    model: [[f32; 4]; 4],
) -> FrameMesh<'a> {
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
}

fn scene<'a>(f: &'a Fixture, post: &'a PostSettings, vis: bool) -> FrameScene<'a> {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.visibility_buffer = vis;
    quality.render_scale_permille = 1000;
    quality.taau = false;
    let mut meshes = vec![
        mesh(1, 1, &f.ground, &f.floor, translate(0.0, -0.05, 0.0)),
        mesh(2, 2, &f.wall, &f.floor, translate(0.0, 1.5, -3.0)),
    ];
    for i in 0..TREES {
        let model = translate((i as f32 - 3.0) * 0.9, 0.0, -((i % 3) as f32) * 0.5);
        meshes.push(mesh(3, 100 + i as u64, &f.trunk, &f.bark, model));
        meshes.push(mesh(4, 200 + i as u64, &f.crown, &f.leaf, model));
    }
    FrameScene {
        width: SIZE,
        height: SIZE,
        eye: [0.0, 1.2, 3.6],
        target: [0.0, 1.1, -1.0],
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
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.4, -1.0, -0.5], [1.0; 3], 100_000.0),
        local_lights: &[],
        quality,
        meshes,
    }
}

fn renderer(device: &RenderDevice) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    if device.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        renderer.set_material_anisotropy_for_tests(1);
    }
    renderer.set_meshlet_builder(Some(cooked_builder()));
    renderer
}

fn diff(a: &[u8], b: &[u8]) -> (u8, f64) {
    let mut worst = 0u8;
    let mut close = 0usize;
    for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = p
            .iter()
            .zip(q)
            .take(3)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0);
        worst = worst.max(d);
        close += usize::from(d <= 2);
    }
    (worst, close as f64 / (a.len() / 4) as f64)
}

fn floors(device: &RenderDevice) -> f64 {
    if device.capabilities.backend == wgpu::Backend::Metal {
        STRICT.min(0.97)
    } else {
        STRICT
    }
}

/// A swaying masked tree on the path against the forward path at fixed wind times: the differential's
/// strict floor, every plant on the path, the leaves alpha-tested. The negative control is the same
/// frame in calm air.
#[test]
fn a_swaying_masked_tree_matches_the_forward_path_at_fixed_wind_times() {
    let Some(device) = device_or_skip() else {
        return;
    };
    let f = fixture(true);
    let settings = post();
    let (forward_scene, vis_scene) = (scene(&f, &settings, false), scene(&f, &settings, true));
    let wind = [7.0, 2.0];
    let mut forward = renderer(&device);
    let mut vis = renderer(&device);
    let mut calm = renderer(&device);
    for (frame, (now, prev)) in [(0.5, 0.45), (0.5, 0.5), (1.25, 1.2), (2.9, 2.85), (3.7, 3.65)]
        .into_iter()
        .enumerate()
    {
        for (r, w) in [(&mut forward, wind), (&mut vis, wind), (&mut calm, [0.0, 0.0])] {
            r.set_material_clock(now, prev);
            r.set_foliage_wind(Some(w));
        }
        let reference = forward
            .render_offscreen(&device, &forward_scene)
            .expect("frame")
            .pixels;
        let pixels = vis.render_offscreen(&device, &vis_scene).expect("frame").pixels;
        let at_rest = calm.render_offscreen(&device, &vis_scene).expect("frame").pixels;
        if frame < 2 {
            continue;
        }
        let (worst, close) = diff(&reference, &pixels);
        let ssim = ssim_rgba8(&reference, &pixels, SIZE, SIZE).expect("ssim");
        let stats = vis.visbuffer_stats();
        println!(
            "frame {frame}: {:.3} % within 2 levels, worst {worst}, ssim {ssim:.4}, {stats:?}",
            close * 100.0
        );
        assert!(
            close >= floors(&device),
            "frame {frame}: {:.3} % close",
            close * 100.0
        );
        assert!(ssim >= floors(&device), "frame {frame}: ssim {ssim}");
        assert_eq!(
            stats.sway_instances as usize,
            2 * TREES,
            "every plant is on the path"
        );
        assert_eq!(stats.sway_materials, 2);
        assert_eq!(stats.masked_instances as usize, TREES);
        assert!(stats.masked_tested, "the leaves are alpha-tested");
        assert!(stats.sway_moving);
        assert_eq!(stats.sway_forward, [0; SwaySkip::ALL.len()]);
        let (_, unmoved) = diff(&at_rest, &pixels);
        assert!(
            unmoved < 0.97,
            "the wind moves the frame: {:.3} %",
            unmoved * 100.0
        );
    }
}

/// Calm air draws the frame of the same materials without sway, byte for byte (the leaves through the
/// masked raster, their shadows through the forward casters).
#[test]
fn zero_wind_is_bit_identical_to_the_same_tree_without_sway() {
    let Some(device) = device_or_skip() else {
        return;
    };
    let settings = post();
    let (swaying, plain) = (fixture(true), fixture(false));
    let (a, b) = (scene(&swaying, &settings, true), scene(&plain, &settings, true));
    let mut with = renderer(&device);
    let mut without = renderer(&device);
    for frame in 0..4 {
        let t = 1.0 + frame as f32 * 0.25;
        for r in [&mut with, &mut without] {
            r.set_material_clock(t, t - 0.25);
            r.set_foliage_wind(Some([0.0, 0.0]));
        }
        let (p, q) = (
            with.render_offscreen(&device, &a).expect("frame").pixels,
            without.render_offscreen(&device, &b).expect("frame").pixels,
        );
        let (worst, close) = diff(&p, &q);
        assert!(
            p == q,
            "frame {frame}: {:.3} % close, worst {worst}",
            close * 100.0
        );
    }
    let stats = with.visbuffer_stats();
    assert_eq!(stats.sway_instances as usize, 2 * TREES);
    assert!(!stats.sway_moving);
}

/// With the clock running, the cluster shadow of a swaying caster moves with it: the dynamic-caster
/// class redraws its pages, and the frame stays within the floor of the forward caster's. The class
/// off holds the shadow still.
#[test]
fn a_swaying_cluster_caster_refreshes_its_pages_and_follows_the_forward_shadow() {
    let Some(device) = device_or_skip() else {
        return;
    };
    let f = fixture(true);
    let settings = post();
    // Trunks only: opaque, so they cast as clusters.
    let opaque = |vis: bool| {
        let mut s = scene(&f, &settings, vis);
        s.meshes.retain(|m| m.mesh_key != 4);
        s
    };
    let wind = [14.0, 0.0];
    let mut forward = renderer(&device);
    let mut vis = renderer(&device);
    let mut off = renderer(&device);
    off.set_dynamic_caster_shadows(regia_render::DynamicCasterShadows::Off);
    for (frame, (now, prev)) in [(0.5, 0.45), (0.5, 0.5), (1.25, 1.2), (2.9, 2.85)]
        .into_iter()
        .enumerate()
    {
        for r in [&mut forward, &mut vis, &mut off] {
            r.set_material_clock(now, prev);
            r.set_foliage_wind(Some(wind));
        }
        let reference = forward
            .render_offscreen(&device, &opaque(false))
            .expect("frame")
            .pixels;
        let pixels = vis
            .render_offscreen(&device, &opaque(true))
            .expect("frame")
            .pixels;
        let held = off
            .render_offscreen(&device, &opaque(true))
            .expect("frame")
            .pixels;
        if frame < 2 {
            continue;
        }
        let report = vis.dynamic_caster_report();
        println!("frame {frame}: {report:?}");
        assert!(
            report.dynamic_pages > 0,
            "the swaying casters' pages redraw: {report:?}"
        );
        assert_eq!(report.animated_casters as usize, TREES);
        let (worst, close) = diff(&reference, &pixels);
        println!("frame {frame}: {:.3} % close, worst {worst}", close * 100.0);
        assert!(
            close >= floors(&device),
            "frame {frame}: {:.3} % close",
            close * 100.0
        );
        let (_, same) = diff(&held, &pixels);
        assert!(same < 1.0, "the class on differs from the class off: {same}");
    }
}

/// A warm swaying frame creates nothing through the path, with the clock and the wind moving.
#[test]
fn a_warm_swaying_masked_frame_creates_nothing() {
    let Some(device) = device_or_skip() else {
        return;
    };
    let f = fixture(true);
    let settings = post();
    let s = scene(&f, &settings, true);
    let mut vis = renderer(&device);
    for frame in 0..8 {
        let t = frame as f32 * 0.37;
        vis.set_material_clock(t, t - 0.37);
        vis.set_foliage_wind(Some([5.0 + t, 1.0]));
        vis.render_offscreen(&device, &s).expect("frame");
        if frame >= 3 {
            let last = vis.visbuffer_create_stats().last_frame;
            let objects = last.pipelines + last.bind_groups + last.buffers + last.textures;
            assert_eq!(objects, 0, "frame {frame}: {last:?}");
        }
    }
}

/// What stays forward says why, and what the path cannot take still draws.
#[test]
fn materials_that_stay_forward_report_their_reason() {
    let Some(device) = device_or_skip() else {
        return;
    };
    let settings = post();
    // A sway on a surface graph of its own.
    let mut custom = fixture(true);
    custom.leaf = LabMaterial {
        // The standard surface under another text: a graph the gate cannot call the adopted one.
        surface_graph: Some(Arc::new(sway().graph(&format!(
            "{}\n// a surface of its own\n",
            regia_render::FOLIAGE_SURFACE
        )))),
        ..LabMaterial::default()
    };
    let s = scene(&custom, &settings, true);
    let mut vis = renderer(&device);
    vis.set_foliage_wind(Some([6.0, 0.0]));
    vis.render_offscreen(&device, &s).expect("frame");
    let stats = vis.visbuffer_stats();
    assert_eq!(stats.sway_forward[SwaySkip::GraphSurface.index()] as usize, TREES);
    assert_eq!(stats.sway_instances as usize, TREES, "the trunks are on the path");

    // A masked sway on a frame that hashes its alpha test.
    let f = fixture(true);
    let mut hashed = scene(&f, &settings, true);
    hashed.quality.taau = true;
    hashed.quality.render_scale_permille = 750;
    let hashed_post = PostSettings { taa: true, ..post() };
    hashed.post = Some(&hashed_post);
    let mut vis = renderer(&device);
    vis.set_taa_hashed_alpha(true);
    vis.set_foliage_wind(Some([6.0, 0.0]));
    vis.render_offscreen(&device, &hashed).expect("frame");
    let stats = vis.visbuffer_stats();
    assert_eq!(stats.sway_forward[SwaySkip::HashedAlpha.index()] as usize, TREES);
    assert_eq!(stats.sway_instances as usize, TREES);

    // A masked sway on a device without the table; the opaque trunks stay on the path.
    let plain_device = RenderDevice::headless().expect("device");
    if regia_render::texture_table::device_support(&plain_device.device)
        .capacity()
        .is_none()
        || !plain_device
            .device
            .features()
            .contains(wgpu::Features::TEXTURE_BINDING_ARRAY)
    {
        let s = scene(&f, &settings, true);
        let mut vis = renderer(&plain_device);
        vis.set_foliage_wind(Some([6.0, 0.0]));
        vis.render_offscreen(&plain_device, &s).expect("frame");
        let stats = vis.visbuffer_stats();
        assert_eq!(
            stats.sway_forward[SwaySkip::NoTextureTable.index()] as usize,
            TREES
        );
        assert_eq!(stats.sway_instances as usize, TREES);
    }
}

/// The set path's own error against the forward path, which a swaying set must not exceed by more
/// than this under wind. A persistent set differs from the same trees as a mesh list by a few levels
/// on 2 to 6 % of this fixture's pixels with or without sway (the set path, not the sway: lane
/// diagnostic, 11-06), so a swaying set is held to the plain set's closeness, not to the floor.
const SET_MARGIN: f64 = 0.005;

/// A swaying persistent set's shadow pages are in the dynamic class too (its box joins it): redrawn
/// under wind, kept in calm air. Calm air draws the plain set's frame byte for byte; under wind the
/// swaying set is as close to the forward path as the plain set is (its shadow follows the clock in
/// every page) and no shadow work is dropped.
#[test]
fn a_swaying_set_refreshes_its_shadow_pages_under_wind() {
    let Some(device) = device_or_skip() else {
        return;
    };
    let settings = post();
    let models: Vec<[[f32; 4]; 4]> = (0..TREES)
        .map(|i| translate((i as f32 - 3.0) * 0.9, 0.0, -((i % 3) as f32) * 0.5))
        .collect();
    let (swaying, plain) = (fixture(true), fixture(false));
    // The trees as a persistent set on the path, and as a mesh list on the forward path.
    let set_renderer = |f: &Fixture| {
        let mut r = renderer(&device);
        let set = regia_render::visbuffer::VisInstanceSet {
            mesh_key: 3,
            vertices: &f.trunk.0,
            indices: &f.trunk.1,
            material_key: 1003,
            material: &f.bark,
            models: &models,
            shadow: ShadowCast::All,
            instance_shadows: &[],
        };
        assert!(r.set_instance_set(7, &set));
        r
    };
    let set_scene = |f| {
        let mut s = scene(f, &settings, true);
        s.meshes.truncate(2);
        s
    };
    let listed = |f| {
        let mut l = scene(f, &settings, false);
        l.meshes.truncate(2);
        for (i, model) in models.iter().enumerate() {
            l.meshes.push(mesh(3, 300 + i as u64, &f.trunk, &f.bark, *model));
        }
        l
    };
    let (s_sway, s_plain) = (set_scene(&swaying), set_scene(&plain));
    let (l_sway, l_plain) = (listed(&swaying), listed(&plain));
    let mut vis = set_renderer(&swaying);
    let mut still = set_renderer(&plain);
    let mut forward = renderer(&device);
    let mut forward_still = renderer(&device);
    let mut frames = Vec::new();
    for (i, wind) in [
        [0.0, 0.0],
        [0.0, 0.0],
        [0.0, 0.0],
        [12.0, 0.0],
        [12.0, 0.0],
        [12.0, 0.0],
    ]
    .into_iter()
    .enumerate()
    {
        let t = 1.0 + i as f32 * 0.4;
        for r in [&mut vis, &mut still, &mut forward, &mut forward_still] {
            r.set_material_clock(t, t - 0.4);
            r.set_foliage_wind(Some(wind));
        }
        frames.push(vis.render_offscreen(&device, &s_sway).expect("frame").pixels);
        let calm = still.render_offscreen(&device, &s_plain).expect("frame").pixels;
        let reference = forward.render_offscreen(&device, &l_sway).expect("frame").pixels;
        let reference_still = forward_still
            .render_offscreen(&device, &l_plain)
            .expect("frame")
            .pixels;
        let (worst, close) = diff(&reference, &frames[i]);
        let (worst_still, close_still) = diff(&reference_still, &calm);
        println!(
            "frame {i}: swaying set {:.3} % close (worst {worst}), plain set {:.3} % (worst {worst_still})",
            close * 100.0,
            close_still * 100.0
        );
        if wind == [0.0, 0.0] {
            assert!(
                frames[i] == calm,
                "frame {i}: calm air draws the plain set's frame"
            );
        }
        if i >= 1 {
            // Its shadow follows the clock, in every page: a frame's jobs hold the set's work items.
            assert!(
                close >= close_still - SET_MARGIN,
                "frame {i}: {:.3} % close, the plain set {:.3} %",
                close * 100.0,
                close_still * 100.0
            );
            let [.., lost_clusters, lost_work] = vis
                .visbuffer_shadow_counters(&device)
                .expect("readback")
                .unwrap_or_default();
            assert_eq!((lost_clusters, lost_work), (0, 0), "frame {i}");
        }
        let report = vis.dynamic_caster_report();
        println!(
            "frame {i} wind {wind:?}: {report:?} sway {:?}",
            vis.visbuffer_stats().sway_moving
        );
        if i == 2 {
            assert_eq!(report.dynamic_pages, 0, "calm air keeps the pages");
        }
        if i >= 4 {
            assert!(report.dynamic_pages > 0, "wind redraws them: {report:?}");
        }
    }
    assert!(frames[4] != frames[5], "the frame moves with the clock");
}
