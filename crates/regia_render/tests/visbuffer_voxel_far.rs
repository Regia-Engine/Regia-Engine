// SPDX-License-Identifier: MPL-2.0
//! Voxel far proxies on the visibility buffer (ADR-2445), on whatever adapter
//! `RenderDevice::headless` picks (lavapipe in CI).
//!
//! A foliage canopy (disjoint leaf quads the simplifier cannot merge) is cooked as the cook does
//! it (meshopt cluster DAG, `voxel_far_cook::voxelize`), extended with its voxel levels
//! (`VoxelFarProxy::extend_meshlets`) and drawn as a persistent instance set. Checks:
//!
//! - **nothing changes before the switch**: nearer than the first voxel level's switch distance
//!   the extended mesh draws the same frame as the plain DAG;
//! - **the voxels take over past it**: farther, the cut draws fewer clusters than the plain DAG
//!   and the canopy keeps its screen coverage and color (no hole, no pop);
//! - **shadows follow**: with a coarse shadow LOD threshold the shadow jobs draw the voxel
//!   clusters (fewer than the plain DAG's) and the frame still matches in coverage.
//!
//! Counts and coverage are printed (`--nocapture`); GPU times are not asserted.

use std::sync::Arc;

use regia_asset_pipeline::voxel_far_cook::{VoxelFarCookSettings, VoxelizeInput, voxelize};
use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::visbuffer::{
    MeshletMesh, VisBufferSettings, VisInstanceSet, VoxelFarProxy, VoxelFarSettings,
};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
};

const SIZE: u32 = 128;
const FOV: f32 = 1.0;
const MESH_KEY: u64 = 77;
const SET_KEY: u64 = 5;
/// Cluster LOD threshold of these frames, pixels: at 2 px the canopy is about 30 pixels across
/// where its first voxel level takes over, enough to compare coverage.
const THRESHOLD_PX: f32 = 2.0;

/// The visibility settings of these tests: [`THRESHOLD_PX`] and the hard LOD cut
/// (`lod_hysteresis_px: Some(0.0)`). The tests ask where the voxel levels switch and what they
/// cost at that cut, and several move one renderer's camera a few percent across the switch.
/// The default band (ADR-4631) holds the previous level through such a move and widens each
/// instance's level range by the band, which is its purpose; `lod_hysteresis` tests the band,
/// and `near_canopies_spend_no_cull_work_on_the_voxel_levels` measures it on the voxel levels.
fn hard_cut() -> VisBufferSettings {
    VisBufferSettings {
        lod_threshold_px: THRESHOLD_PX,
        lod_hysteresis_px: Some(0.0),
        ..VisBufferSettings::default()
    }
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// `count` leaf quads in a unit ball (fixed LCG), facing out from the centre, green vertex color.
/// Disjoint leaves are what the simplifier cannot merge without collapsing them: the cluster
/// DAG's roots keep every leaf, so the voxel levels start from a fine one.
fn canopy(count: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let mut state = 0x1234_5677u32;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32
    };
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for _ in 0..count {
        let (mut c, mut l2) = ([0.0f32; 3], 2.0f32);
        while l2 > 1.0 {
            c = [next() * 2.0 - 1.0, next() * 2.0 - 1.0, next() * 2.0 - 1.0];
            l2 = c[0] * c[0] + c[1] * c[1] + c[2] * c[2];
        }
        let len = l2.sqrt().max(1e-3);
        let n = c.map(|v| v / len);
        let a = if n[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        let t = cross(n, a);
        let tl = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
        let t = t.map(|v| v / tl * 0.04);
        let b = cross(n, t);
        let base = vertices.len() as u32;
        for (u, v) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let mut vertex = LabVertex::from_pos_normal_uv(
                std::array::from_fn(|k| c[k] * 0.9 + t[k] * u + b[k] * v),
                n,
                [0.5, 0.5],
            );
            vertex.color = [0.18, 0.45, 0.08, 1.0];
            vertices.push(vertex);
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

struct Fixture {
    vertices: Vec<LabVertex>,
    indices: Vec<u32>,
    dag: Arc<MeshletMesh>,
    full: Arc<MeshletMesh>,
    far_vertices: Vec<LabVertex>,
    far_dag: Arc<MeshletMesh>,
    first_switch: f32,
    first_triangles: u32,
    /// The cooked proxy (format v2, with sub-voxel boxes).
    proxy: VoxelFarProxy,
    root_triangles: u32,
    ground: (Vec<LabVertex>, Vec<u32>),
    leaf: LabMaterial,
    floor: LabMaterial,
}

fn fixture() -> Fixture {
    let (vertices, indices) = canopy(3000);
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
    let colors: Vec<[f32; 4]> = vertices.iter().map(|v| v.color).collect();
    let dag = build_meshlet_mesh_with_attributes(
        &positions,
        Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        }),
        &indices,
        &MeshletSettings::default(),
    )
    .expect("cluster DAG");
    let proxy = voxelize(
        "canopy",
        &VoxelizeInput {
            positions: &positions,
            normals: &normals,
            uvs: &uvs,
            colors: &colors,
            indices: &indices,
        },
        &VoxelFarCookSettings { resolution: 32 },
    )
    .expect("voxelize");
    let far = proxy
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    let first = *far.levels.first().expect("a voxel level cheaper than the roots");
    let radius = far
        .meshlets
        .meshlets
        .iter()
        .find(|m| m.level >= first.dag_level)
        .map_or(1.0, |m| m.lod[3]);
    let ppu = SIZE as f32 / (2.0 * (FOV * 0.5).tan());
    let full = Arc::new(MeshletMesh::single_level(&positions, &indices));
    Fixture {
        first_switch: VoxelFarProxy::switch_distance(first.error, radius, ppu, THRESHOLD_PX),
        first_triangles: first.triangles,
        root_triangles: far.root_triangles,
        proxy,
        vertices,
        indices,
        dag: Arc::new(dag),
        full,
        far_vertices: far.vertices,
        far_dag: Arc::new(far.meshlets),
        ground: regia_render::box_mesh([40.0, 0.05, 40.0]),
        leaf: LabMaterial::default(),
        floor: LabMaterial::default(),
    }
}

/// Pixels per unit of the test viewport.
fn ppu() -> f32 {
    SIZE as f32 / (2.0 * (FOV * 0.5).tan())
}

/// The first used voxel level's switch distance of `far`.
fn switch_of(far: &regia_render::visbuffer::VoxelFarMesh) -> f32 {
    let first = far.levels[0];
    let radius = far
        .meshlets
        .meshlets
        .iter()
        .find(|m| m.level >= first.dag_level)
        .map_or(1.0, |m| m.lod[3]);
    VoxelFarProxy::switch_distance(first.error, radius, ppu(), THRESHOLD_PX)
}

/// `proxy` as format v1 drew it: whole cubes, each level's error the cube diagonal.
fn as_whole_cubes(proxy: &VoxelFarProxy) -> VoxelFarProxy {
    let mut cubes = proxy.clone();
    cubes.extents.clear();
    cubes.sampled_errors.clear();
    for level in &mut cubes.levels {
        level.error = level.voxel_size * 3f32.sqrt();
    }
    cubes
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

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

/// The ground as the frame's one mesh, the camera `distance` from the canopy's centre.
fn scene<'a>(f: &'a Fixture, post: &'a PostSettings, distance: f32) -> FrameScene<'a> {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.visibility_buffer = true;
    // Native resolution: the cull projects errors with the render height, so a reduced render
    // scale would move the switch nearer than the viewport's prediction.
    quality.render_scale_permille = 1000;
    quality.taau = false;
    // The square viewport keeps the given vertical field of view (horizontal-plus would widen it
    // from the 16:9 reference, lowering the pixels per unit the cut uses).
    quality.horizontal_plus = false;
    FrameScene {
        width: SIZE,
        height: SIZE,
        eye: [0.0, 1.6, distance],
        target: [0.0, 1.4, 0.0],
        fov_y: FOV,
        near: 0.1,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 100_000.0),
        local_lights: &[],
        quality,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1001,
            object_key: 1,
            vertices: &f.ground.0,
            indices: &f.ground.1,
            material: &f.floor,
            model: translate(0.0, -0.05, 0.0),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    }
}

const MODELS: [[[f32; 4]; 4]; 1] = [[
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 1.4, 0.0, 1.0],
]];

/// Which clusters a renderer draws the canopy with.
#[derive(Clone, Copy)]
enum Dag {
    /// The cooked cluster DAG.
    Plain,
    /// The DAG with the voxel levels on top.
    Extended,
    /// Every source triangle at every distance (the full-detail reference).
    Full,
}

/// A renderer drawing the canopy as an instance set.
fn renderer(f: &Fixture, which: Dag, settings: VisBufferSettings) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_visbuffer_settings(settings);
    let (vertices, dag) = match which {
        Dag::Plain => (&f.vertices, &f.dag),
        Dag::Extended => (&f.far_vertices, &f.far_dag),
        Dag::Full => (&f.vertices, &f.full),
    };
    renderer.insert_meshlets(MESH_KEY, Arc::clone(dag));
    let set = VisInstanceSet {
        mesh_key: MESH_KEY,
        vertices,
        indices: &f.indices,
        material_key: 2001,
        material: &f.leaf,
        models: &MODELS,
        shadow: ShadowCast::All,
        instance_shadows: &[],
    };
    assert!(renderer.set_instance_set(SET_KEY, &set), "instance set accepted");
    renderer
}

struct Shot {
    pixels: Vec<u8>,
    clusters: u32,
    shadow_clusters: u32,
}

fn shoot(renderer: &mut FrameRenderer, device: &RenderDevice, scene: &FrameScene<'_>) -> Shot {
    // Two frames: the second has last frame's visibility for phase 1.
    renderer.render_offscreen(device, scene).expect("frame");
    let pixels = renderer.render_offscreen(device, scene).expect("frame").pixels;
    let counters = renderer
        .visbuffer_counters(device)
        .expect("readback")
        .expect("counters");
    let shadow = renderer
        .visbuffer_shadow_counters(device)
        .expect("readback")
        .unwrap_or([0; 4]);
    Shot {
        pixels,
        clusters: counters.phase1_clusters + counters.phase2_clusters + counters.software_clusters,
        shadow_clusters: shadow[1],
    }
}

/// (pixels where `shot` differs from `empty` by more than 6 levels, their mean rgb).
fn canopy_pixels(shot: &[u8], empty: &[u8]) -> (usize, [f64; 3]) {
    let mut count = 0usize;
    let mut sum = [0.0f64; 3];
    for (p, e) in shot.as_chunks::<4>().0.iter().zip(empty.as_chunks::<4>().0) {
        if p.iter().zip(e).take(3).any(|(a, b)| a.abs_diff(*b) > 6) {
            count += 1;
            for k in 0..3 {
                sum[k] += f64::from(p[k]);
            }
        }
    }
    (count, sum.map(|s| s / count.max(1) as f64))
}

/// Pixels where `shot` differs from `empty` by more than 6 levels.
fn mask(shot: &[u8], empty: &[u8]) -> Vec<bool> {
    shot.as_chunks::<4>()
        .0
        .iter()
        .zip(empty.as_chunks::<4>().0)
        .map(|(p, e)| p.iter().zip(e).take(3).any(|(a, b)| a.abs_diff(*b) > 6))
        .collect()
}

/// Pixels of `a` farther than `radius` pixels (Chebyshev) from every pixel of `b`.
fn outside(a: &[bool], b: &[bool], radius: i32) -> usize {
    let n = SIZE as i32;
    let at = |x: i32, y: i32| x >= 0 && y >= 0 && x < n && y < n && b[(y * n + x) as usize];
    (0..n * n)
        .filter(|&i| a[i as usize])
        .filter(|&i| {
            let (x, y) = (i % n, i / n);
            !(-radius..=radius).any(|dy| (-radius..=radius).any(|dx| at(x + dx, y + dy)))
        })
        .count()
}

fn empty_frame(device: &RenderDevice, scene: &FrameScene<'_>) -> Vec<u8> {
    let mut plain = FrameRenderer::new();
    plain.render_offscreen(device, scene).expect("frame");
    plain.render_offscreen(device, scene).expect("frame").pixels
}

fn close_fraction(a: &[u8], b: &[u8]) -> f64 {
    let close = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(x, y)| x.iter().zip(y.iter()).all(|(p, q)| p.abs_diff(*q) <= 2))
        .count();
    close as f64 / (a.len() / 4) as f64
}

#[test]
fn voxel_levels_draw_past_the_switch_and_nothing_changes_before_it() {
    let device = device();
    let f = fixture();
    let settings = post();
    println!(
        "voxel_far render: root cut {} tris, first voxel level {} tris, switch at {:.2} m ({} px viewport)",
        f.root_triangles, f.first_triangles, f.first_switch, SIZE
    );
    assert!(
        f.first_switch > 2.0 && f.first_switch < 300.0,
        "{}",
        f.first_switch
    );

    // Nearer than the switch: the same frame as the plain DAG.
    let near = scene(&f, &settings, f.first_switch * 0.8);
    let vis = hard_cut();
    let mut plain = renderer(&f, Dag::Plain, vis);
    let mut extended = renderer(&f, Dag::Extended, vis);
    let mut full = renderer(&f, Dag::Full, vis);
    let a = shoot(&mut plain, &device, &near);
    let b = shoot(&mut extended, &device, &near);
    // The full-detail canopy draws the same frames in the same order as the other two: an instance
    // set is in the probes' scene (ADR-5457), so the canopy's indirect shadow on the ground builds
    // up over a renderer's first frames, and a renderer two frames behind differs from the empty
    // frame over a smaller patch of ground (12 px of it past the silhouette bound at x0.98).
    shoot(&mut full, &device, &near);
    let close = close_fraction(&a.pixels, &b.pixels);
    println!(
        "  x0.8: clusters {} vs {}, {:.3}% close",
        a.clusters,
        b.clusters,
        close * 100.0
    );
    assert_eq!(
        a.clusters, b.clusters,
        "before the switch the cut is the plain DAG's"
    );
    assert!(close > 0.999, "before the switch the frames match ({close})");

    // Just either side of the switch, and well past it, against the full-detail canopy.
    let luma = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    // The silhouette may move by the threshold (the LOD error bound) plus a pixel of raster
    // rounding.
    let radius = THRESHOLD_PX.ceil() as i32 + 1;
    for factor in [0.98f32, 1.02, 1.5, 2.0] {
        let s = scene(&f, &settings, f.first_switch * factor);
        let empty = empty_frame(&device, &s);
        let a = shoot(&mut plain, &device, &s);
        let b = shoot(&mut extended, &device, &s);
        let c = shoot(&mut full, &device, &s);
        let (area_a, rgb_a) = canopy_pixels(&a.pixels, &empty);
        let (area_b, rgb_b) = canopy_pixels(&b.pixels, &empty);
        let (area_c, rgb_c) = canopy_pixels(&c.pixels, &empty);
        println!(
            "  x{factor}: clusters plain {} voxel {} full {}; canopy px {area_a} / {area_b} / {area_c}; \
             rgb {rgb_a:.0?} / {rgb_b:.0?} / {rgb_c:.0?}",
            a.clusters, b.clusters, c.clusters
        );
        assert!(area_c > 20, "the canopy is on screen at x{factor} ({area_c} px)");
        if factor > 1.0 {
            assert!(
                b.clusters < a.clusters,
                "past the switch the voxels draw fewer clusters"
            );
        } else {
            assert_eq!(a.clusters, b.clusters);
            assert!(close_fraction(&a.pixels, &b.pixels) > 0.999);
        }
        // No hole and no blow-up: every voxel pixel lies within the error bound of the
        // full-detail canopy, and every full-detail pixel within it of the voxels.
        let (vm, fm) = (mask(&b.pixels, &empty), mask(&c.pixels, &empty));
        let (grown, holes) = (outside(&vm, &fm, radius), outside(&fm, &vm, radius));
        println!(
            "    silhouette: {grown} voxel px and {holes} full-detail px beyond {radius} px of the other"
        );
        assert_eq!(
            (grown, holes),
            (0, 0),
            "x{factor}: the voxels stay within the error bound"
        );
        let rel = (luma(rgb_b) - luma(rgb_c)).abs() / luma(rgb_c).max(1.0);
        assert!(
            rel < 0.35,
            "x{factor}: canopy luminance differs from full detail by {:.0}%",
            rel * 100.0
        );
        assert!(
            rgb_b[1] > rgb_b[0] && rgb_b[1] > rgb_b[2],
            "the voxels keep the leaves' green"
        );
    }
}

#[test]
fn shadow_jobs_cast_the_voxel_levels() {
    let device = device();
    let f = fixture();
    let settings = post();
    // A coarse shadow threshold puts the casters on the voxel levels at this range.
    let coarse = VisBufferSettings {
        shadow_lod_threshold_px: 64.0,
        ..hard_cut()
    };
    let s = scene(&f, &settings, f.first_switch * 0.8);
    let mut plain = renderer(&f, Dag::Plain, coarse);
    let mut extended = renderer(&f, Dag::Extended, coarse);
    let a = shoot(&mut plain, &device, &s);
    let b = shoot(&mut extended, &device, &s);
    let empty = empty_frame(&device, &s);
    let (area_a, _) = canopy_pixels(&a.pixels, &empty);
    let (area_b, _) = canopy_pixels(&b.pixels, &empty);
    // The main view's cut is the same; only the cast shadow (and the canopy's self-shadowing)
    // changes, which is a small part of the frame.
    let close = close_fraction(&a.pixels, &b.pixels);
    println!(
        "voxel_far shadows: shadow clusters plain {} voxel {}; canopy and shadow px {} vs {}; {:.2}% close",
        a.shadow_clusters,
        b.shadow_clusters,
        area_a,
        area_b,
        close * 100.0
    );
    assert!(
        a.shadow_clusters > 0,
        "the canopy casts through the cluster shadows"
    );
    assert!(
        b.shadow_clusters > 0 && b.shadow_clusters < a.shadow_clusters,
        "the voxel levels cast with fewer clusters"
    );
    assert_eq!(a.clusters, b.clusters, "the main view still draws the triangles");
    assert!(
        area_a > 20 && area_b > 20,
        "both frames show the canopy and its shadow"
    );
    assert!(close > 0.9, "only the shadow changes ({close})");
}

fn env_usize(name: &str, default: usize) -> usize {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

fn median(values: &mut [f32]) -> f32 {
    values.sort_by(f32::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

/// One cost run of the canopies at `models` as an instance set: (clusters drawn, shadow clusters,
/// median GPU frame ms, arena bytes, cull work items).
fn cost_run(
    device: &RenderDevice,
    f: &Fixture,
    s: &FrameScene<'_>,
    models: &[[[f32; 4]; 4]],
    extended: bool,
    frames: usize,
    vis: VisBufferSettings,
) -> (u32, u32, f32, u64, u32) {
    let mut renderer = FrameRenderer::new();
    renderer.set_visbuffer_settings(vis);
    renderer.set_gpu_timing(device, true);
    let (vertices, dag) = if extended {
        (&f.far_vertices, &f.far_dag)
    } else {
        (&f.vertices, &f.dag)
    };
    renderer.insert_meshlets(MESH_KEY, Arc::clone(dag));
    let set = VisInstanceSet {
        mesh_key: MESH_KEY,
        vertices,
        indices: &f.indices,
        material_key: 2001,
        material: &f.leaf,
        models,
        shadow: ShadowCast::All,
        instance_shadows: &[],
    };
    assert!(renderer.set_instance_set(SET_KEY, &set));
    let mut ms = Vec::with_capacity(frames);
    for _ in 0..frames {
        renderer.render_offscreen(device, s).expect("frame");
        ms.push(renderer.gpu_timings().frame_ms);
    }
    let counters = renderer
        .visbuffer_counters(device)
        .expect("readback")
        .expect("counters");
    let shadow = renderer
        .visbuffer_shadow_counters(device)
        .expect("readback")
        .unwrap_or([0; 4]);
    let stats = renderer.visbuffer_arena_stats();
    (
        counters.phase1_clusters + counters.phase2_clusters + counters.software_clusters,
        shadow[1],
        median(&mut ms),
        stats.used_bytes,
        counters.work_items,
    )
}

/// Canopies nearer than the first voxel level's switch distance need none of the voxel levels:
/// with the per-instance level range (ADR-3915) their cull work is the plain DAG's, item for item
/// (before it, every voxel cluster was a candidate of every instance), and they draw the same
/// clusters.
#[test]
fn near_canopies_spend_no_cull_work_on_the_voxel_levels() {
    let device = device();
    let f = fixture();
    let settings = post();
    let n = 64;
    let side = 8;
    let (near, far) = (f.first_switch * 0.3, f.first_switch * 0.8);
    let models: Vec<[[f32; 4]; 4]> = (0..n)
        .map(|i| {
            let (row, col) = (i / side, i % side);
            let z = near + (far - near) * row as f32 / side as f32;
            let x = (col as f32 - side as f32 * 0.5) * z * 0.9 / side as f32;
            let mut m = MODELS[0];
            m[3] = [x, 1.4, -z + f.first_switch, 1.0];
            m
        })
        .collect();
    let s = scene(&f, &settings, f.first_switch);
    let (plain_clusters, _, _, _, plain_items) = cost_run(&device, &f, &s, &models, false, 3, hard_cut());
    let (voxel_clusters, _, _, _, voxel_items) = cost_run(&device, &f, &s, &models, true, 3, hard_cut());
    // The same still view with the default LOD band (ADR-4631): after the first frame each
    // instance's level range widens by the band so a held cluster stays a candidate.
    let band = VisBufferSettings {
        lod_hysteresis_px: None,
        ..hard_cut()
    };
    let (band_clusters, _, _, _, band_items) = cost_run(&device, &f, &s, &models, true, 3, band);
    println!(
        "near canopies ({n}, {near:.0}-{far:.0} m): work items {plain_items} -> {voxel_items}, clusters \
         {plain_clusters} -> {voxel_clusters}; default band ({:.3} px): work items {band_items}, clusters \
         {band_clusters}",
        band.resolved_lod_hysteresis_px()
    );
    assert_eq!(
        voxel_clusters, plain_clusters,
        "no voxel level is selected this near"
    );
    assert_eq!(
        voxel_items, plain_items,
        "the near canopies' work items are the plain DAG's"
    );
    // ADR-4631: a still camera keeps the hard cut cluster for cluster.
    assert_eq!(
        band_clusters, plain_clusters,
        "the band selects no voxel level for a still camera"
    );
    assert!(
        band_items < 2 * plain_items,
        "the band's wider level range stays under the unranged cost ({band_items} against {plain_items})"
    );
}

/// A forest row past the switch: `VOXEL_FAR_N` canopies (default 64) on a grid from 1.2× to 4×
/// the first switch distance, plain DAG vs extended. Asserts the cluster and shadow-cluster
/// counts drop; prints GPU frame times, which mean something on a real GPU only:
///
/// ```text
/// VOXEL_FAR_N=4000 VOXEL_FAR_FRAMES=40 cargo test -p regia_render --release \
///     --test visbuffer_voxel_far voxel_far_cost -- --nocapture --test-threads=1
/// ```
#[test]
fn voxel_far_cost() {
    let device = device();
    let f = fixture();
    let settings = post();
    let n = env_usize("VOXEL_FAR_N", 64);
    let frames = env_usize("VOXEL_FAR_FRAMES", 4);
    let side = (n as f32).sqrt().ceil() as usize;
    let (near, far) = (f.first_switch * 1.2, f.first_switch * 4.0);
    let mut models = Vec::with_capacity(n);
    for i in 0..n {
        let (row, col) = (i / side, i % side);
        let z = near + (far - near) * row as f32 / side.max(2) as f32;
        let x = (col as f32 - side as f32 * 0.5) * z * 0.9 / side as f32;
        let mut m = MODELS[0];
        m[3] = [x, 1.4, -z + f.first_switch, 1.0];
        models.push(m);
    }
    let s = scene(&f, &settings, f.first_switch);
    let run = |extended: bool| cost_run(&device, &f, &s, &models, extended, frames, hard_cut());
    let (plain_clusters, plain_shadow, plain_ms, plain_bytes, plain_items) = run(false);
    let (voxel_clusters, voxel_shadow, voxel_ms, voxel_bytes, voxel_items) = run(true);
    println!(
        "voxel_far cost ({n} canopies, {:.0}-{:.0} m, {} px): clusters {plain_clusters} -> {voxel_clusters}, \
         shadow clusters {plain_shadow} -> {voxel_shadow}, GPU frame {plain_ms:.3} -> {voxel_ms:.3} ms \
         (adapter {:?}), arena {plain_bytes} -> {voxel_bytes} B, cull work items {plain_items} -> {voxel_items}",
        near,
        far,
        SIZE,
        device.adapter.get_info().device_type,
    );
    assert!(
        voxel_clusters < plain_clusters,
        "the far canopies draw fewer clusters"
    );
    assert!(voxel_shadow <= plain_shadow, "and no more shadow clusters");
    assert!(
        voxel_bytes > plain_bytes,
        "the voxel levels are resident once per mesh"
    );
}

/// The largest silhouette distance, pixels (Chebyshev, 0 to 12), between two canopy masks: every
/// pixel of each lies within it of the other.
fn silhouette_px(a: &[bool], b: &[bool]) -> i32 {
    (0..=12)
        .find(|&r| outside(a, b, r) == 0 && outside(b, a, r) == 0)
        .unwrap_or(13)
}

/// A renderer drawing `vertices` / `dag` as the canopy's instance set.
fn renderer_with(
    f: &Fixture,
    vertices: &[LabVertex],
    dag: &Arc<MeshletMesh>,
    models: &[[[f32; 4]; 4]],
) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_visbuffer_settings(hard_cut());
    renderer.insert_meshlets(MESH_KEY, Arc::clone(dag));
    let set = VisInstanceSet {
        mesh_key: MESH_KEY,
        vertices,
        indices: &f.indices,
        material_key: 2001,
        material: &f.leaf,
        models,
        shadow: ShadowCast::All,
        instance_shadows: &[],
    };
    assert!(renderer.set_instance_set(SET_KEY, &set), "instance set accepted");
    renderer
}

/// Format v2's sub-voxel boxes (ADR-2655) against whole cubes (v1) on the canopy, lavapipe
/// differential against the full-detail canopy: the boxes take over nearer, by a counted margin,
/// and at their own switch the silhouette stays within the same pixel bound as the cubes at
/// theirs (the LOD threshold plus a pixel of raster rounding).
#[test]
fn sub_voxel_boxes_switch_nearer_at_the_same_silhouette_error() {
    let device = device();
    let f = fixture();
    let settings = post();
    let cubes = as_whole_cubes(&f.proxy);
    let loose = cubes
        .extend_meshlets(&f.dag, &f.vertices, &VoxelFarSettings::default())
        .expect("extend");
    let tight = f
        .proxy
        .extend_meshlets(&f.dag, &f.vertices, &VoxelFarSettings::default())
        .expect("extend");
    let (d_box, d_cube) = (switch_of(&tight), switch_of(&loose));
    println!(
        "voxel_far boxes: level {} error {:.4} switches at {d_box:.2} m; cubes level {} error {:.4} at \
         {d_cube:.2} m; {:.2} m ({:.1} %) nearer",
        tight.levels[0].level,
        tight.levels[0].error,
        loose.levels[0].level,
        loose.levels[0].error,
        d_cube - d_box,
        (1.0 - d_box / d_cube) * 100.0
    );
    assert!(d_box < d_cube * 0.9, "the boxes switch at least 10 % nearer");
    let tight_dag = Arc::new(tight.meshlets.clone());
    let loose_dag = Arc::new(loose.meshlets.clone());
    let bound = THRESHOLD_PX.ceil() as i32 + 1;
    let mut plain = renderer_with(&f, &f.vertices, &f.dag, &MODELS);
    let mut full = renderer_with(&f, &f.vertices, &f.full, &MODELS);
    let mut boxes = renderer_with(&f, &tight.vertices, &tight_dag, &MODELS);
    let mut whole = renderer_with(&f, &loose.vertices, &loose_dag, &MODELS);
    let mut worst = [0i32; 2];
    // Between the two switches the boxes draw voxels and the cubes still draw triangles.
    for (factor, at) in [
        (1.02f32, d_box),
        (0.5 * (1.0 + d_cube / d_box), d_box),
        (1.02, d_cube),
    ] {
        let s = scene(&f, &settings, at * factor);
        let empty = empty_frame(&device, &s);
        let p = shoot(&mut plain, &device, &s);
        let c = shoot(&mut full, &device, &s);
        let b = shoot(&mut boxes, &device, &s);
        let w = shoot(&mut whole, &device, &s);
        let fm = mask(&c.pixels, &empty);
        let (sb, sw) = (
            silhouette_px(&mask(&b.pixels, &empty), &fm),
            silhouette_px(&mask(&w.pixels, &empty), &fm),
        );
        println!(
            "  at {:.2} m: clusters plain {} boxes {} cubes {}; silhouette vs full detail: boxes {sb} px, cubes {sw} px",
            at * factor,
            p.clusters,
            b.clusters,
            w.clusters
        );
        assert!(b.clusters < p.clusters, "past their switch the boxes draw voxels");
        if at * factor < d_cube {
            assert_eq!(
                w.clusters, p.clusters,
                "before theirs the cubes draw the triangles"
            );
        } else {
            assert!(w.clusters < p.clusters);
            worst[1] = worst[1].max(sw);
        }
        worst[0] = worst[0].max(sb);
    }
    println!(
        "  worst silhouette: boxes {} px, cubes {} px (bound {bound} px)",
        worst[0], worst[1]
    );
    assert!(
        worst[0] <= bound && worst[1] <= bound,
        "{worst:?} within {bound} px"
    );
}

/// A placed mesh (a `FrameMesh` with its own vertex buffer and mesh key, as the framework
/// bridge draws an ECS entity) takes the bridged DAG (`VoxelFarMesh::bridged`: voxel vertices
/// inside the meshlets): before and past the switch it draws the same clusters and frame as the
/// instance set with the extended vertex buffer.
#[test]
fn a_placed_mesh_draws_the_bridged_dag_like_an_instance_set() {
    let device = device();
    let f = fixture();
    let settings = post();
    let far = f
        .proxy
        .extend_meshlets(&f.dag, &f.vertices, &VoxelFarSettings::default())
        .expect("extend");
    let bridged = Arc::new(far.bridged());
    assert_eq!(bridged.base_vertex_count(), f.vertices.len());
    for factor in [0.8f32, 1.5] {
        let mut s = scene(&f, &settings, f.first_switch * factor);
        s.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: MESH_KEY,
            material_key: 2001,
            object_key: 2,
            vertices: &f.vertices,
            indices: &f.indices,
            material: &f.leaf,
            model: MODELS[0],
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
        let mut placed = FrameRenderer::new();
        placed.set_visbuffer_settings(hard_cut());
        placed.insert_meshlets(MESH_KEY, Arc::clone(&bridged));
        let a = shoot(&mut placed, &device, &s);
        assert!(placed.has_meshlets(MESH_KEY), "resident, not declined");
        let s = scene(&f, &settings, f.first_switch * factor);
        let far_dag = Arc::new(far.meshlets.clone());
        let mut set = renderer_with(&f, &far.vertices, &far_dag, &MODELS);
        let b = shoot(&mut set, &device, &s);
        let close = close_fraction(&a.pixels, &b.pixels);
        println!(
            "voxel_far placed x{factor}: clusters placed {} set {}; {:.3} % close",
            a.clusters,
            b.clusters,
            close * 100.0
        );
        assert_eq!(a.clusters, b.clusters, "x{factor}: the same cut");
        assert!(close > 0.99, "x{factor}: the same frame ({close})");
    }
}

/// Triangles of the CPU LOD cut of `dag` over `models` from the test eye (no frustum or
/// occlusion culling: what the cut selects).
fn cut_triangles(dag: &MeshletMesh, models: &[[[f32; 4]; 4]], eye: [f32; 3]) -> u64 {
    models
        .iter()
        .map(|m| {
            let view = regia_render::visbuffer::LodView {
                eye: [eye[0] - m[3][0], eye[1] - m[3][1], eye[2] - m[3][2]],
                pixels_per_unit: ppu(),
                threshold_px: THRESHOLD_PX,
                near: 0.1,
            };
            dag.lod_cut(&view)
                .map(|i| u64::from(dag.meshlets[i].triangle_count))
                .sum::<u64>()
        })
        .sum()
}

/// `n` canopies on a grid from `near` to `far` times the first switch distance.
fn grid_models(f: &Fixture, n: usize, near: f32, far: f32) -> Vec<[[f32; 4]; 4]> {
    let side = (n as f32).sqrt().ceil() as usize;
    let (near, far) = (f.first_switch * near, f.first_switch * far);
    (0..n)
        .map(|i| {
            let (row, col) = (i / side, i % side);
            let z = near + (far - near) * row as f32 / side.max(2) as f32;
            let x = (col as f32 - side as f32 * 0.5) * z * 0.9 / side as f32;
            let mut m = MODELS[0];
            m[3] = [x, 1.4, -z + f.first_switch, 1.0];
            m
        })
        .collect()
}

/// The counted cost table of `book/agents/voxel-far.md` (ADR-2655): 64 canopies in a near-only,
/// far-only and mixed view, plain DAG against the DAG with the boxed voxel levels: cull work
/// items, clusters drawn and shadow clusters (GPU counters), triangles of the LOD cut, and arena
/// bytes. Near-only views are unchanged, far-only ones cut fewer triangles and clusters; every
/// view pays the arena bytes. With the per-instance level range (ADR-3915) the voxel levels are
/// cull candidates only of the canopies that can select them: near-only views pay at most a few
/// items (the range is conservative for canopies close to the switch), far-only views do less cull
/// work than the plain DAG (they skip its fine levels). Before the range every view paid twice.
#[test]
fn voxel_far_cost_table() {
    let device = device();
    let f = fixture();
    let settings = post();
    let s = scene(&f, &settings, f.first_switch);
    let eye = s.eye;
    let far = f
        .proxy
        .extend_meshlets(&f.dag, &f.vertices, &VoxelFarSettings::default())
        .expect("extend");
    let far_dag = Arc::new(far.meshlets.clone());
    println!("| View | DAG | Work items | Clusters | Shadow clusters | Cut triangles | Arena bytes |");
    println!("|---|---|---|---|---|---|---|");
    for (view, near, farther) in [
        ("near-only", 0.3f32, 0.9f32),
        ("far-only", 1.2, 4.0),
        ("mixed", 0.3, 4.0),
    ] {
        let models = grid_models(&f, 64, near, farther);
        let mut rows = Vec::new();
        for (name, vertices, dag) in [("plain", &f.vertices, &f.dag), ("voxel", &far.vertices, &far_dag)] {
            let mut renderer = renderer_with(&f, vertices, dag, &models);
            let shot = shoot(&mut renderer, &device, &s);
            let items = renderer
                .visbuffer_counters(&device)
                .expect("readback")
                .expect("counters")
                .work_items;
            let bytes = renderer.visbuffer_arena_stats().used_bytes;
            let triangles = cut_triangles(dag, &models, eye);
            println!(
                "| {view} | {name} | {items} | {} | {} | {triangles} | {bytes} |",
                shot.clusters, shot.shadow_clusters
            );
            rows.push((items, shot.clusters, shot.shadow_clusters, triangles, bytes));
        }
        let (p, v) = (rows[0], rows[1]);
        assert!(v.4 > p.4, "{view}: the voxel levels are resident once per mesh");
        // The unranged cull paid each instance's whole list: twice the plain DAG's work items here.
        assert!(
            v.0 < 2 * p.0,
            "{view}: the level range keeps work items under the unranged cost"
        );
        match view {
            "near-only" => {
                assert_eq!(v.1, p.1, "near-only: the same clusters");
                assert_eq!(v.3, p.3, "near-only: the same cut");
                assert!(
                    v.0 <= p.0 + models.len() as u32 / 16,
                    "near-only: only canopies close to the switch pay for a voxel level ({} against {})",
                    v.0,
                    p.0
                );
            }
            _ => {
                assert!(v.1 < p.1, "{view}: fewer clusters");
                assert!(v.3 < p.3, "{view}: fewer cut triangles");
            }
        }
        if view == "far-only" {
            assert!(v.0 < p.0, "far-only: the far canopies skip the fine levels");
        }
    }
}
