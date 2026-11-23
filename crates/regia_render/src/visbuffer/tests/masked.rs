// SPDX-License-Identifier: MPL-2.0
//! Masked clusters in the cull's lists and the per-instance level range (ADR-3915).
//!
//! Masked instances' work items follow the opaque ones', so each list holds its opaque clusters
//! first and the masked ones after them, in (instance, cluster) order on every run, and no masked
//! cluster takes the software range. The level range drops work items an instance's distance
//! cannot select, and changes no frame.

use super::super::gpu::CullLists;
use super::super::{Meshlet, VisBufferStats, VisInstanceSet};
use super::*;

/// `state` words of `vis_cull.wgsl` the checks read.
const DS_ARGS: usize = 16;
const SOFT_END: usize = 31;
const KEPT_SINGLE: usize = 45;
const KEPT_DOUBLE: usize = 47;
const MASK_ARGS: usize = 52;

/// A device with the texture table, where masked sets draw as masked clusters.
fn table_device() -> Option<crate::RenderDevice> {
    crate::texture_table::frame_tests::pair().map(|(table, _)| table)
}

fn leaf(double_sided: bool) -> LabMaterial {
    LabMaterial {
        albedo: Some(checker(16)),
        alpha_mode: crate::ALPHA_MASK,
        alpha_cutoff: 0.5,
        double_sided,
        ..LabMaterial::default()
    }
}

/// 16 models in a row in front of the fixture's rocks, at `z`.
fn row(z: f32) -> Vec<[[f32; 4]; 4]> {
    (0..16)
        .map(|i| translate((i as f32 - 7.5) * 0.5, 0.4, z, 0.25))
        .collect()
}

const SPHERE_KEY: u64 = 9;

/// The rock scene's third frame with two masked sets (one double-sided) through a fresh renderer.
fn masked_lists(device: &crate::RenderDevice, settings: VisBufferSettings) -> (CullLists, VisBufferStats) {
    let f = fixture(6);
    let post = post();
    let s = scene(&f, &post, true, false);
    let mut renderer = comparison_renderer(device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    renderer.set_visbuffer_settings(settings);
    let (one, two) = (leaf(false), leaf(true));
    let (front, back) = (row(1.0), row(-1.0));
    for (key, material_key, material, models) in [(1, 3001, &one, &front), (2, 3002, &two, &back)] {
        let set = VisInstanceSet {
            mesh_key: SPHERE_KEY,
            vertices: &f.sphere.0,
            indices: &f.sphere.1,
            material_key,
            material,
            models,
            shadow: ShadowCast::All,
            instance_shadows: &[],
        };
        assert!(renderer.set_instance_set(key, &set), "set {key} accepted");
    }
    for _ in 0..3 {
        render(&mut renderer, device, &s);
    }
    let lists = renderer
        .visbuffer_pass_mut()
        .read_cull_lists(device)
        .expect("readback")
        .expect("a visibility frame ran");
    (lists, renderer.visbuffer_stats())
}

fn entry(lists: &CullLists, slot: usize) -> (u32, u32) {
    (lists.clusters[2 * slot], lists.clusters[2 * slot + 1])
}

/// Each phase's lists: the opaque part, then the masked part, each strictly increasing; the masked
/// entries are the sets' instances (the persistent ones, `masked` of them, come first in the
/// instance buffer) and the opaque ones the mesh list's. Returns the masked clusters listed.
fn assert_masked_after_opaque(name: &str, lists: &CullLists, masked: u32) -> usize {
    let cap = lists.capacity as usize;
    let word = |i: usize| lists.state[i] as usize;
    let mut listed = 0;
    for phase in 0..2 {
        for (double, opaque, kept, masked_args) in [
            (
                false,
                word(phase * 4 + 1),
                word(KEPT_SINGLE + phase),
                word(MASK_ARGS + phase * 8 + 1),
            ),
            (
                true,
                word(DS_ARGS + phase * 4 + 1),
                word(KEPT_DOUBLE + phase),
                word(MASK_ARGS + phase * 8 + 5),
            ),
        ] {
            assert!(
                opaque <= kept,
                "{name}: phase {phase} opaque {opaque} past the list's {kept}"
            );
            assert_eq!(
                kept - opaque,
                masked_args,
                "{name}: phase {phase} masked draw count"
            );
            let at = |i: usize| {
                if double {
                    entry(lists, phase * cap + cap - 1 - i)
                } else {
                    entry(lists, phase * cap + i)
                }
            };
            let part = |range: std::ops::Range<usize>| range.map(at).collect::<Vec<_>>();
            let (front, back) = (part(0..opaque), part(opaque..kept));
            for (what, entries, want_masked) in [("opaque", &front, false), ("masked", &back, true)] {
                for pair in entries.windows(2) {
                    assert!(pair[0] < pair[1], "{name}: {what} part out of order: {pair:?}");
                }
                for &(instance, _) in entries.iter() {
                    assert_eq!(
                        instance < masked,
                        want_masked,
                        "{name}: phase {phase} {what} part (double-sided {double}) lists instance {instance}"
                    );
                }
            }
            listed += back.len();
        }
    }
    listed
}

/// The masked sets' clusters follow the opaque rocks' in each list, in order, the same on every
/// run; with the software raster on none of them takes the software range.
#[test]
fn masked_clusters_follow_the_opaque_ones_in_every_list() {
    let Some(device) = table_device() else {
        eprintln!("skip: no texture-table device");
        return;
    };
    let (lists, stats) = masked_lists(&device, VisBufferSettings::default());
    assert_eq!(stats.masked_instances, 32, "both sets are masked instances");
    assert_eq!(stats.masked_materials, 2);
    let listed = assert_masked_after_opaque("hardware", &lists, 32);
    assert!(listed > 0, "masked clusters listed");
    let (again, _) = masked_lists(&device, VisBufferSettings::default());
    assert_eq!(
        lists.state, again.state,
        "the cull's counters differ between runs"
    );
    assert!(lists.clusters == again.clusters, "the lists differ between runs");

    let software = VisBufferSettings {
        software_raster: true,
        software_raster_max_px: 64,
        ..VisBufferSettings::default()
    };
    let (lists, _) = masked_lists(&device, software);
    assert_masked_after_opaque("software", &lists, 32);
    let cap = lists.capacity as usize;
    let soft = lists.state[SOFT_END] as usize;
    assert!(soft > 0, "the rocks take the software range");
    for k in 0..soft {
        let (instance, _) = entry(&lists, 2 * cap + k);
        assert!(instance >= 32, "masked instance {instance} in the software range");
    }
}

/// `positions`/`indices` as two levels of the same triangles: level 0 under a parent of error
/// `error`, level 1 the root with that error (both cover the mesh exactly, so a frame does not
/// change by which one draws).
fn two_levels(positions: &[[f32; 3]], indices: &[u32], error: f32) -> MeshletMesh {
    let mut mesh = MeshletMesh::single_level(positions, indices);
    let coarse: Vec<Meshlet> = mesh
        .meshlets
        .iter()
        .map(|m| Meshlet {
            lod: mesh.bounds,
            lod_error: error,
            parent: mesh.bounds,
            parent_error: crate::visbuffer::ROOT_ERROR,
            level: 1,
            ..*m
        })
        .collect();
    for m in &mut mesh.meshlets {
        m.parent = mesh.bounds;
        m.parent_error = error;
    }
    mesh.meshlets.extend(coarse);
    mesh
}

fn two_level_builder(error: f32) -> MeshletBuilder {
    Arc::new(move |vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(two_levels(&positions, indices, error))
    })
}

/// (third frame, work items, clusters drawn) of the rock rows with two-level rocks.
fn level_frame(device: &crate::RenderDevice, levels: bool, eye_z: f32) -> (Vec<u8>, u32, u32) {
    let mut f = fixture(10);
    // Over 64 clusters per level (a work item's worth), so a skipped level drops work items.
    f.sphere = crate::uv_sphere(1.0, 160, 80);
    let post = post();
    let mut s = scene(&f, &post, true, false);
    s.eye = [0.0, 2.5, eye_z];
    let mut renderer = comparison_renderer(device);
    if !levels {
        renderer.visbuffer_pass_mut().levels_off = true;
    }
    // 0.05 world units of error: at 1 px and this view the rocks switch about 6 m out, inside
    // the rows.
    renderer.set_meshlet_builder(Some(two_level_builder(0.05)));
    let mut pixels = Vec::new();
    for _ in 0..3 {
        pixels = render(&mut renderer, device, &s);
    }
    let counters = renderer
        .visbuffer_counters(device)
        .expect("readback")
        .expect("counters");
    (
        pixels,
        counters.work_items,
        counters.phase1_clusters + counters.phase2_clusters,
    )
}

/// Near rocks skip their coarse level's work items and far ones their fine level's, and the
/// frame is the one without the range byte for byte.
#[test]
fn the_level_range_drops_work_items_and_changes_no_frame() {
    let device = crate::test_device();
    for eye_z in [4.0, 9.0] {
        let (with, with_items, with_clusters) = level_frame(&device, true, eye_z);
        let (without, without_items, without_clusters) = level_frame(&device, false, eye_z);
        eprintln!(
            "eye z {eye_z}: work items {without_items} -> {with_items}, clusters {without_clusters} -> {with_clusters}"
        );
        assert_eq!(
            with_clusters, without_clusters,
            "eye z {eye_z}: the same clusters draw"
        );
        assert!(with == without, "eye z {eye_z}: the frame changed");
        assert!(
            with_items < without_items,
            "eye z {eye_z}: the range drops work items ({without_items} -> {with_items})"
        );
    }
}

/// A strip of `n` x `n` quads whose long axis (local `(1, -1, 0)`, half length 1) the sheared model
/// below stretches to world X, half length 4. Its width (world Z, half 0.25) is narrow, so the mesh's
/// reach is about its half length, and the columns' 2.92 understates what the far ends get.
fn strip(n: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let mut vertices = Vec::new();
    for j in 0..=n {
        for i in 0..=n {
            let (u, v) = (i as f32 / n as f32, j as f32 / n as f32);
            let a = 2.0 * u - 1.0;
            vertices.push(LabVertex::from_pos_normal_uv(
                [a * half, -a * half, 0.5 * v - 0.25],
                [half, half, 0.0],
                [u, v],
            ));
        }
    }
    let mut indices = Vec::new();
    for j in 0..n {
        for i in 0..n {
            let at = |x: u32, y: u32| y * (n + 1) + x;
            indices.extend([at(i, j), at(i + 1, j), at(i, j + 1)]);
            indices.extend([at(i + 1, j), at(i + 1, j + 1), at(i, j + 1)]);
        }
    }
    (vertices, indices)
}

/// Two levels of the same triangles, where each cluster's parent sphere is its own: unlike
/// [`two_levels`], the level switch depends on where the cluster is.
fn own_sphere_level_builder(error: f32) -> MeshletBuilder {
    Arc::new(move |vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let mut mesh = MeshletMesh::single_level(&positions, indices);
        let coarse: Vec<Meshlet> = mesh
            .meshlets
            .iter()
            .map(|m| Meshlet {
                lod_error: error,
                level: 1,
                ..*m
            })
            .collect();
        for m in &mut mesh.meshlets {
            m.parent_error = error;
        }
        mesh.meshlets.extend(coarse);
        Some(mesh)
    })
}

/// `(frame, clusters, work items)` of the strip under a non-uniform parent over a rotated child,
/// `eye_x` from its center along the stretched axis. Both levels hold the same triangles, and each
/// cluster picks its level from a sphere on the cluster itself, so a level switch runs across the
/// strip and a range that drops a level still in use leaves a hole.
fn sheared_frame(device: &crate::RenderDevice, levels: bool, eye_x: f32) -> (Vec<u8>, u32, u32) {
    let f = fixture(1);
    let post = post();
    let surface = strip(96);
    let material = LabMaterial {
        albedo: Some(checker(32)),
        double_sided: true,
        ..LabMaterial::default()
    };
    let sheared = (regia_core::glam::Mat4::from_scale(regia_core::glam::Vec3::new(4.0, 1.0, 1.0))
        * regia_core::glam::Mat4::from_rotation_z(std::f32::consts::FRAC_PI_4))
    .to_cols_array_2d();
    let mut s = scene(&f, &post, true, false);
    s.meshes = vec![FrameMesh {
        is_static: false,
        mesh_key: 20,
        material_key: 1020,
        object_key: 20,
        vertices: &surface.0,
        indices: &surface.1,
        material: &material,
        model: sheared,
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::Off,
        uv1: &[],
        lod_dither: 0.0,
    }];
    s.eye = [eye_x, 1.0, 0.0];
    s.target = [0.0, 0.0, 0.0];
    // At the stretch bound (4) and 1 px this error switches levels about 8 m from a cluster, so the
    // switch crosses the strip for most of the eye positions below.
    let pixels_per_unit = s.height as f32 / (2.0 * (s.fov_y * 0.5).tan());
    let error = 8.0 / (4.0 * pixels_per_unit);
    let mut renderer = comparison_renderer(device);
    if !levels {
        renderer.visbuffer_pass_mut().levels_off = true;
    }
    renderer.set_meshlet_builder(Some(own_sphere_level_builder(error)));
    let mut pixels = Vec::new();
    for _ in 0..3 {
        pixels = render(&mut renderer, device, &s);
    }
    let counters = renderer
        .visbuffer_counters(device)
        .expect("readback")
        .expect("counters");
    (
        pixels,
        counters.phase1_clusters + counters.phase2_clusters,
        counters.work_items,
    )
}

/// The range holds for a transform whose columns understate its stretch: its reaches use the
/// stretch bound (`max_stretch`), so the near end of a strip keeps the fine level it selects.
#[test]
fn a_sheared_instance_keeps_every_level_it_selects() {
    let device = crate::test_device();
    let mut work = None;
    for eye_x in [6.0, 9.0, 9.5, 10.0, 11.5, 24.0] {
        let (with, with_clusters, with_items) = sheared_frame(&device, true, eye_x);
        let (without, without_clusters, without_items) = sheared_frame(&device, false, eye_x);
        eprintln!(
            "eye x {eye_x}: clusters {without_clusters} -> {with_clusters}, work items {without_items} -> {with_items}"
        );
        assert!(without_clusters > 0, "eye x {eye_x}: the strip draws");
        assert_eq!(
            with_clusters, without_clusters,
            "eye x {eye_x}: the same clusters draw"
        );
        assert!(with == without, "eye x {eye_x}: the frame changed");
        work = Some((with_items, without_items));
    }
    // From far away only the coarse level can be selected: the range is really on.
    let (with_items, without_items) = work.expect("ran");
    assert!(
        with_items < without_items,
        "far work items {without_items} -> {with_items}"
    );
}

/// A masked set on a device without the table draws nothing: the frame is the one without the set.
#[test]
fn a_masked_set_without_the_table_draws_nothing() {
    let Some((_, device)) = crate::texture_table::frame_tests::pair() else {
        eprintln!("skip: no device pair");
        return;
    };
    let f = fixture(2);
    let post = post();
    let s = scene(&f, &post, true, false);
    let material = leaf(false);
    let models = row(1.0);
    let frame = |with_set: bool| {
        let mut renderer = comparison_renderer(&device);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        if with_set {
            let set = VisInstanceSet {
                mesh_key: SPHERE_KEY,
                vertices: &f.sphere.0,
                indices: &f.sphere.1,
                material_key: 3001,
                material: &material,
                models: &models,
                shadow: ShadowCast::All,
                instance_shadows: &[],
            };
            assert!(renderer.set_instance_set(1, &set), "the set is stored");
        }
        let mut pixels = Vec::new();
        for _ in 0..3 {
            pixels = render(&mut renderer, &device, &s);
        }
        (pixels, renderer.visbuffer_stats().masked_instances)
    };
    let (without, _) = frame(false);
    let (with, masked) = frame(true);
    assert_eq!(masked, 0, "no masked instance without the table");
    assert!(with == without, "the set draws nothing");
}

/// The table stays flat while meshes come and go (compaction: `gpu.rs`
/// `a_level_compaction_moves_the_runs_with_their_meshes`).
#[test]
fn the_level_table_holds_every_resident_mesh() {
    let device = crate::test_device();
    let f = fixture(2);
    let post = post();
    let s = scene(&f, &post, true, false);
    let mut renderer = comparison_renderer(&device);
    renderer.set_meshlet_builder(Some(two_level_builder(0.05)));
    render(&mut renderer, &device, &s);
    let first = renderer.visbuffer_pass_mut().level_records();
    assert!(first >= 2, "the two-level rocks have runs");
    for _ in 0..2 {
        render(&mut renderer, &device, &s);
    }
    assert_eq!(
        renderer.visbuffer_pass_mut().level_records(),
        first,
        "a warm frame adds no runs"
    );
}

/// `spirv-val` and the explicit-layout scan (ADR-026) on the alpha-tested raster, which
/// `shader_check` cannot run on its gate device (no binding-array features), and naga's own
/// validation with every capability.
#[test]
fn the_masked_raster_module_passes_spirv_val() {
    let module = crate::shader_lib::compose_source(
        super::super::gpu::masked_raster_source(),
        "vis_raster_masked.wgsl",
        super::super::gpu::masked_defs(4096),
    )
    .expect("compose");
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap_or_else(|err| panic!("{err:?}"));
    crate::shader_check::spirv_validate("vis_raster_masked.wgsl", &module)
        .unwrap_or_else(|err| panic!("{err}"));
}
