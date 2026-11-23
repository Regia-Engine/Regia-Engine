// SPDX-License-Identifier: MPL-2.0
//! Wind on the runtime paths (lavapipe in CI, ADR-1238): a material graph whose vertex stage
//! sways each instance by `sin(time + object position)`, the way foliage wind is authored.
//!
//! - a field of swaying instances draws the same frame on the GPU-driven instanced path as on
//!   the per-object path, at several clock values;
//! - the frame is a function of the material clock alone: two fresh renderers at the same clock
//!   draw the same bytes, and so does the clock one or more laps later
//!   ([`regia_render::material_clock`]);
//! - an offset that carries an instance from outside the view into it still draws it: the
//!   culling bounds grow by the graph's `max_vertex_offset` on both paths.

use std::collections::BTreeMap;
use std::time::Duration;

use regia_matgraph::{MaterialGraph, compile, default_params, lab_material};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabMesh, LabScene,
    LabVertex, MATERIAL_CLOCK_PERIOD_SECS, RenderDevice, material_clock,
};

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"))
}

/// (largest channel difference, pixels differing by more than `tolerance`).
fn compare(a: &[u8], b: &[u8], tolerance: u8) -> (u8, usize) {
    assert_eq!(a.len(), b.len());
    let mut max = 0u8;
    let mut over = 0usize;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        max = max.max(d);
        if d > tolerance {
            over += 1;
        }
    }
    (max, over)
}

/// A cube centered at the origin (24 vertices, outward normals, tangents along +U).
fn cube(half: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (n, u, v) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [
                (n[0] + u[0] * su + v[0] * sv) * half,
                (n[1] + u[1] * su + v[1] * sv) * half,
                (n[2] + u[2] * su + v[2] * sv) * half,
            ];
            let mut vertex = LabVertex::from_pos_normal_uv(p, n, [(su + 1.0) * 0.5, (1.0 - sv) * 0.5]);
            vertex.tangent = [u[0], u[1], u[2], 1.0];
            vertices.push(vertex);
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

/// Ground (material 0) under a low sun, eye looking down at the origin.
fn stage(width: u32, height: u32) -> LabScene {
    let ground = [
        LabVertex::from_pos_normal_uv([-12.0, 0.0, -12.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([12.0, 0.0, -12.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([12.0, 0.0, 12.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-12.0, 0.0, 12.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    LabScene {
        width,
        height,
        eye: [0.0, 6.0, 9.0],
        target: [0.0, 0.5, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 15_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.55, -0.45, -0.35], [1.0, 0.96, 0.9], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.55, 0.5, 0.45, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: ground.to_vec(),
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn add_cube(scene: &mut LabScene, material: usize, model: [[f32; 4]; 4]) {
    let (vertices, indices) = cube(0.4);
    scene.meshes.push(LabMesh {
        vertices,
        indices,
        material,
        model,
        ..LabMesh::unskinned()
    });
}

/// Instancing needs `INDIRECT_FIRST_INSTANCE` + multi-draw indirect.
fn gpu_driven(device: &RenderDevice) -> bool {
    device.capabilities.multi_draw_indirect && device.capabilities.indirect_first_instance
}

fn quality(gpu: bool) -> FrameQuality {
    FrameQuality {
        gpu_driven: gpu,
        ..FrameQuality::LAB
    }
}

/// One mesh key for every cube: the instances share one GPU mesh and batch together.
fn share_cube_mesh(frame: &mut FrameScene<'_>) {
    for mesh in frame.meshes.iter_mut().skip(1) {
        mesh.mesh_key = 0xC0BE;
    }
}

/// Wind: each instance sways by `gust · sin(time + x + z)` of its origin, a rigid sway with a
/// per-instance phase (at most 0.37 m, under the 0.5 m bound).
const WIND: &str = r#"(
    format_version: 2,
    params: [(name: "gust", value: Vec3((0.35, 0.0, 0.12)))],
    nodes: {
        "c": Vec3((0.3, 0.55, 0.2)),
        "t": Time,
        "op": ObjectPosition,
        "opx": Swizzle(input: "op", channels: "x"),
        "opz": Swizzle(input: "op", channels: "z"),
        "phase_a": Add("t", "opx"),
        "phase": Add("phase_a", "opz"),
        "s": Sin("phase"),
        "gust": Param("gust"),
        "sway": Mul("gust", "s"),
    },
    output: (base_color: Some("c"), world_position_offset: Some("sway")),
    shading: (max_vertex_offset: 0.5),
)"#;

fn wind_material() -> LabMaterial {
    let graph = MaterialGraph::from_ron(WIND).expect("parse wind graph");
    let compiled = compile(&graph).unwrap_or_else(|e| panic!("{e}"));
    lab_material(&graph, &compiled, default_params(&graph, 0.0), &BTreeMap::new()).expect("wind material")
}

/// Sixteen swaying cubes on the ground.
fn field() -> LabScene {
    let mut scene = stage(256, 192);
    scene.materials.push(wind_material());
    for i in 0..16 {
        let (x, z) = ((i % 4) as f32 * 1.4 - 2.1, (i / 4) as f32 * 1.4 - 2.1);
        add_cube(&mut scene, 1, translate(x, 0.4, z));
    }
    scene
}

/// One 60 Hz frame of gameplay time.
const FRAME: Duration = Duration::from_nanos(16_666_667);

/// Renders `scene` once with a fresh renderer at gameplay time `elapsed`.
fn render_at(device: &RenderDevice, scene: &LabScene, gpu: bool, elapsed: Duration) -> (Vec<u8>, usize) {
    let mut frame = scene.as_frame_scene_with_quality(quality(gpu));
    share_cube_mesh(&mut frame);
    let mut renderer = FrameRenderer::new();
    let (now, prev) = material_clock(elapsed, FRAME);
    renderer.set_material_clock(now, prev);
    let image = renderer.render_offscreen(device, &frame).expect("frame");
    (image.pixels, renderer.draw_stats().graph_instanced)
}

#[test]
fn a_swaying_field_draws_the_same_frame_instanced_and_per_object() {
    let device = device();
    let scene = field();
    for t in [0.0f64, 0.7, 2.3] {
        let elapsed = Duration::from_secs_f64(t);
        let (per_object, _) = render_at(&device, &scene, false, elapsed);
        let (instanced, instanced_count) = render_at(&device, &scene, true, elapsed);
        if !gpu_driven(&device) {
            eprintln!("device has no indirect first-instance: both frames are per-object draws");
            return;
        }
        assert_eq!(instanced_count, 16, "t {t}: every swaying cube draws instanced");
        let (max, over) = compare(&per_object, &instanced, 1);
        assert_eq!(
            over, 0,
            "t {t}: the instanced wind frame must equal the per-object one (max {max}/255)"
        );
    }
}

#[test]
fn the_wind_frame_is_a_function_of_the_material_clock() {
    let device = device();
    let scene = field();
    let t = Duration::from_millis(12_500);
    let gpu = gpu_driven(&device);
    let (a, _) = render_at(&device, &scene, gpu, t);
    let (b, _) = render_at(&device, &scene, gpu, t);
    assert!(a == b, "two renderers at the same clock must draw the same bytes");
    // Laps later the clock reads the same, so the frame is the same.
    let period = Duration::from_secs(MATERIAL_CLOCK_PERIOD_SECS);
    let (c, _) = render_at(&device, &scene, gpu, t + 3 * period);
    assert!(a == c, "the clock three laps later must draw the same bytes");
    // The wind does move things: a quarter period of the sway later the frame differs.
    let (d, _) = render_at(&device, &scene, gpu, t + Duration::from_millis(1_571));
    let (_, moved) = compare(&a, &d, 2);
    assert!(moved > 200, "the sway must be visible ({moved} px)");
}

/// A constant offset carries the cube 6 m to the right; at rest it is outside the view.
const SHOVE: &str = r#"(
    format_version: 2,
    params: [(name: "shove", value: Vec3((6.0, 0.0, 0.0)))],
    nodes: {
        "c": Vec3((0.8, 0.35, 0.1)),
        "shove": Param("shove"),
    },
    output: (base_color: Some("c"), world_position_offset: Some("shove")),
    shading: (max_vertex_offset: 6.5),
)"#;

fn shove_material(offset: bool) -> LabMaterial {
    let mut text = SHOVE.to_string();
    if !offset {
        text = text.replace(", world_position_offset: Some(\"shove\")", "");
    }
    let graph = MaterialGraph::from_ron(&text).expect("parse");
    let compiled = compile(&graph).unwrap_or_else(|e| panic!("{e}"));
    lab_material(&graph, &compiled, default_params(&graph, 0.0), &BTreeMap::new()).expect("material")
}

#[test]
fn an_offset_into_the_view_is_not_culled() {
    let device = device();
    for gpu in [false, true] {
        // A: the cube outside the view on the left, offset into it. B: the cube moved there by
        // its model matrix. C: no cube (control: the cube is visible).
        let mut a = stage(224, 168);
        a.materials.push(shove_material(true));
        add_cube(&mut a, 1, translate(-9.0, 0.4, 0.0));
        let mut b = stage(224, 168);
        b.materials.push(shove_material(false));
        add_cube(&mut b, 1, translate(-3.0, 0.4, 0.0));
        let c = stage(224, 168);
        let render = |scene: &LabScene| {
            FrameRenderer::new()
                .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality(gpu)))
                .expect("frame")
                .pixels
        };
        let (pa, pb, pc) = (render(&a), render(&b), render(&c));
        let tag = if gpu { "gpu" } else { "cpu" };
        let (_, control) = compare(&pc, &pb, 2);
        assert!(
            control > 200,
            "{tag}: the moved cube must be visible ({control} px)"
        );
        let (max, over) = compare(&pa, &pb, 2);
        // Edge pixels may round differently ((m·p) + o vs m'·p), as in `runtime_paths.rs`.
        assert!(
            over <= 40,
            "{tag}: the cube offset into the view differs from the moved cube in {over} px (max {max}); \
             a culled cube differs in about {control}"
        );
    }
}
