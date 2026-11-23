// SPDX-License-Identifier: MPL-2.0
//! Material graphs on the runtime paths (lavapipe in CI):
//!
//! - graph draws take the GPU-driven instanced path like other materials and render the same
//!   frame as the per-object path;
//! - async pipeline builds: a prewarmed graph draws with no pipeline built on the frame
//!   thread, a graph met on the frame path draws with the standard surface until it lands;
//! - a vertex stage (world position offset) moves the surface in the forward, depth and
//!   shadow passes (same frame as moving the mesh) and in the motion vectors (same TAAU
//!   frame as moving the mesh, unlike a mesh moved without its history);
//! - the second UV set reaches `TexCoord1` (same frame as scaling the first set).
//!
//! `REGIA_EVIDENCE=1` writes the frames to `docs/evidence/matgraph-runtime/`; frames always
//! go to `<temp>/matgraph-runtime`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_matgraph::{MaterialGraph, compile, default_params, lab_material};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabMesh, LabScene,
    LabTexture, LabVertex, PostSettings, RenderDevice,
};

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"))
}

fn save(name: &str, width: u32, height: u32, pixels: &[u8]) {
    let file = format!("{name}.png");
    write_preview(
        &std::env::temp_dir().join("matgraph-runtime").join(&file),
        width,
        height,
        pixels,
    );
    if regia_core::brand::env_var("REGIA_EVIDENCE").ok().as_deref() == Some("1") {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/evidence/matgraph-runtime");
        write_png(&dir.join(&file), width, height, pixels);
    }
}

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("evidence dir");
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png pixels");
    writer.finish().expect("png finish");
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

/// A unit cube centred at the origin (24 vertices, outward normals, tangents along +U).
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

fn graph_material(text: &str, images: &BTreeMap<String, LabTexture>) -> LabMaterial {
    let graph = MaterialGraph::from_ron(text).expect("parse graph");
    let compiled = compile(&graph).unwrap_or_else(|e| panic!("{e}"));
    lab_material(&graph, &compiled, default_params(&graph, 0.0), images).expect("graph material")
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

const TINT: &str = r#"(
    format_version: 2,
    params: [(name: "tint", value: Color((0.7, 0.25, 0.1))), (name: "rough", value: Float(0.45))],
    nodes: {
        "tint": Param("tint"),
        "vc": VertexColor,
        "rgb": Swizzle(input: "vc", channels: "rgb"),
        "base": Mul("tint", "rgb"),
        "rough": Param("rough"),
        "wp": WorldPosition,
        "stripe_a": Swizzle(input: "wp", channels: "x"),
        "k": Float(6.0),
        "stripe_b": Mul("stripe_a", "k"),
        "stripe": Sin("stripe_b"),
        "half": Float(0.5),
        "stripe_c": Mul("stripe", "half"),
        "stripe_d": Add("stripe_c", "half"),
        "metal": Mul("stripe_d", "half"),
    },
    output: (base_color: Some("base"), roughness: Some("rough"), metallic: Some("metal")),
)"#;

#[test]
fn graph_draws_take_the_gpu_driven_instanced_path() {
    let device = device();
    let mut scene = stage(256, 192);
    scene.materials.push(graph_material(TINT, &BTreeMap::new()));
    for i in 0..16 {
        let (x, z) = ((i % 4) as f32 * 1.4 - 2.1, (i / 4) as f32 * 1.4 - 2.1);
        add_cube(&mut scene, 1, translate(x, 0.4, z));
    }
    let render = |gpu: bool| {
        let mut frame = scene.as_frame_scene_with_quality(quality(gpu));
        share_cube_mesh(&mut frame);
        let mut renderer = FrameRenderer::new();
        let image = renderer.render_offscreen(&device, &frame).expect("frame");
        (image.pixels, renderer.draw_stats())
    };
    let (per_object, cpu_stats) = render(false);
    assert_eq!(cpu_stats.graph_per_object, 16, "{cpu_stats:?}");
    assert_eq!(cpu_stats.graph_instanced, 0, "{cpu_stats:?}");
    save("instanced_cpu", scene.width, scene.height, &per_object);
    let (instanced, gpu_stats) = render(true);
    save("instanced_gpu", scene.width, scene.height, &instanced);
    if !gpu_driven(&device) {
        eprintln!(
            "device has no indirect first-instance: the GPU-driven path falls back to per-object draws"
        );
        return;
    }
    assert_eq!(gpu_stats.graph_instanced, 16, "{gpu_stats:?}");
    assert_eq!(gpu_stats.graph_per_object, 0, "{gpu_stats:?}");
    let (max, over) = compare(&per_object, &instanced, 1);
    eprintln!("instanced vs per-object graph draws: max {max}/255, {over} px over 1/255");
    assert_eq!(
        over, 0,
        "the instanced graph path must render the per-object frame (max {max})"
    );
}

const STRIPES: &str = r#"(
    format_version: 2,
    params: [(name: "tint", value: Color((0.1, 0.35, 0.8)))],
    nodes: {
        "tint": Param("tint"),
        "wp": WorldPosition,
        "z": Swizzle(input: "wp", channels: "z"),
        "k": Float(9.0),
        "zk": Mul("z", "k"),
        "f": Frac("zk"),
        "base": Mul("tint", "f"),
    },
    output: (base_color: Some("base")),
)"#;

#[test]
fn prewarmed_graphs_build_nothing_on_the_frame_thread() {
    let device = device();
    let mut scene = stage(192, 144);
    scene.materials.push(graph_material(TINT, &BTreeMap::new()));
    let stripes = graph_material(STRIPES, &BTreeMap::new());
    add_cube(&mut scene, 1, translate(-1.0, 0.4, 0.0));
    let gpu = gpu_driven(&device);

    // Frame 1 meets graph A on the frame path: queued, drawn with the standard surface.
    let mut renderer = FrameRenderer::new();
    renderer.set_async_material_graphs(true);
    renderer
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality(gpu)))
        .expect("frame 1");
    let first = renderer.material_graph_stats();
    assert!(first.fallback_draws >= 1, "{first:?}");
    assert_eq!(first.built_on_frame_path, 0, "{first:?}");
    renderer.settle_material_graphs();
    renderer
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality(gpu)))
        .expect("frame 2");
    let second = renderer.material_graph_stats();
    assert_eq!(
        second.fallback_draws, first.fallback_draws,
        "A is ready: {second:?}"
    );

    // Level load: prewarm graph B on the targets the renderer draws, then draw it.
    let targets = renderer.material_graph_targets();
    assert!(!targets.is_empty());
    let b = stripes.surface_graph.as_deref().expect("graph B");
    let started = renderer
        .prewarm_material_graphs(&device, &[b], &targets)
        .expect("prewarm");
    assert!(started >= 1);
    renderer.settle_material_graphs();
    scene.materials.push(stripes);
    add_cube(&mut scene, 2, translate(1.0, 0.4, 0.0));
    let before = regia_render::frame_path_counters();
    let image = renderer
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality(gpu)))
        .expect("frame 3");
    let delta = regia_render::frame_path_counters().since(before);
    let third = renderer.material_graph_stats();
    eprintln!("prewarmed frame: {delta:?}, {third:?}");
    assert_eq!(
        delta.pipelines, 0,
        "a prewarmed graph builds no pipeline on the frame thread"
    );
    assert_eq!(third.built_on_frame_path, 0, "{third:?}");
    assert_eq!(third.fallback_draws, first.fallback_draws, "{third:?}");
    assert!(third.built_off_frame_path >= 2, "{third:?}");

    // Same frame as inline mode, which builds on the frame path (the counter is live).
    let mut inline = FrameRenderer::new();
    let reference = inline
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality(gpu)))
        .expect("inline frame");
    assert!(inline.material_graph_stats().built_on_frame_path >= 2);
    let (max, over) = compare(&image.pixels, &reference.pixels, 0);
    assert_eq!(
        over, 0,
        "prewarmed and inline pipelines draw the same frame (max {max})"
    );
}

const LIFTED: &str = r#"(
    format_version: 2,
    params: [(name: "lift", value: Vec3((0.0, 0.0, 0.0)))],
    nodes: {
        "c": Vec3((0.8, 0.35, 0.1)),
        "lift": Param("lift"),
    },
    output: (base_color: Some("c"), world_position_offset: Some("lift")),
    shading: (max_vertex_offset: 2.0),
)"#;

/// Graph with a constant offset `lift` (or none) and an orange surface.
fn lift_material(lift: Option<[f32; 3]>) -> LabMaterial {
    let mut text = LIFTED.to_string();
    if lift.is_none() {
        text = text.replace(", world_position_offset: Some(\"lift\")", "");
    }
    let graph = MaterialGraph::from_ron(&text).expect("parse");
    let compiled = compile(&graph).unwrap_or_else(|e| panic!("{e}"));
    let mut params = default_params(&graph, 0.0);
    if let Some(lift) = lift {
        params[1] = [lift[0], lift[1], lift[2], 0.0];
    }
    lab_material(&graph, &compiled, params, &BTreeMap::new()).expect("material")
}

#[test]
fn a_vertex_offset_moves_the_surface_its_depth_and_its_shadow() {
    let device = device();
    let lift = [0.3, 1.1, -0.2];
    for gpu in [false, true] {
        // A: cube at rest, lifted by the graph. B: the same surface moved by its model matrix.
        // C: the same surface at rest (control: the lift is visible).
        let mut a = stage(224, 168);
        a.materials.push(lift_material(Some(lift)));
        add_cube(&mut a, 1, translate(0.0, 0.4, 0.0));
        let mut b = stage(224, 168);
        b.materials.push(lift_material(None));
        add_cube(&mut b, 1, translate(lift[0], 0.4 + lift[1], lift[2]));
        let mut c = stage(224, 168);
        c.materials.push(lift_material(None));
        add_cube(&mut c, 1, translate(0.0, 0.4, 0.0));
        let render = |scene: &LabScene| {
            FrameRenderer::new()
                .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality(gpu)))
                .expect("frame")
                .pixels
        };
        let (pa, pb, pc) = (render(&a), render(&b), render(&c));
        let tag = if gpu { "gpu" } else { "cpu" };
        save(&format!("wpo_{tag}_graph"), a.width, a.height, &pa);
        save(&format!("wpo_{tag}_moved"), a.width, a.height, &pb);
        save(&format!("wpo_{tag}_rest"), a.width, a.height, &pc);
        let (max, over) = compare(&pa, &pb, 2);
        let (_, control) = compare(&pc, &pb, 2);
        eprintln!("{tag}: offset vs moved max {max}, {over} px over 2/255; rest vs moved {control} px");
        // Measured on lavapipe: the lift moves ~1 200 px of cube and shadow.
        assert!(control > 500, "the lift must be visible ({control} px)");
        // Edge pixels may round differently ((m·p) + o vs m'·p); the surface, its depth and
        // its shadow on the ground must land in the same place.
        assert!(
            over <= 40,
            "{tag}: the offset surface/shadow differs from the moved mesh in {over} px"
        );
    }
}

const SLIDING: &str = r#"(
    format_version: 2,
    params: [(name: "speed", value: Vec3((0.9, 0.0, 0.0)))],
    nodes: {
        "c": Vec3((0.85, 0.85, 0.8)),
        "t": Time,
        "speed": Param("speed"),
        "slide": Mul("speed", "t"),
    },
    output: (base_color: Some("c"), world_position_offset: Some("slide")),
    shading: (max_vertex_offset: 4.0),
)"#;

#[test]
fn a_time_driven_offset_writes_its_motion_vectors() {
    let device = device();
    let mut q = quality(gpu_driven(&device));
    q.taau = true;
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    };
    let speed = 0.9f32;
    let dt = 1.0 / 30.0;
    let frames = 10;

    // A: rest model, the graph slides the cube with the material clock.
    let mut a = stage(192, 144);
    a.post = Some(post.clone());
    a.materials.push(graph_material(SLIDING, &BTreeMap::new()));
    add_cube(&mut a, 1, translate(-1.0, 0.4, 0.0));
    // B: the same surface moved by its model matrix (motion from the previous model).
    let mut b = stage(192, 144);
    b.post = Some(post.clone());
    b.materials.push(graph_material(
        &SLIDING.replace(", world_position_offset: Some(\"slide\")", ""),
        &BTreeMap::new(),
    ));
    add_cube(&mut b, 1, translate(-1.0, 0.4, 0.0));

    let mut ra = FrameRenderer::new();
    let mut rb = FrameRenderer::new();
    // C: like B, but a new object key every frame, so its motion vectors are zero.
    let mut rc = FrameRenderer::new();
    let (mut pa, mut pb, mut pc) = (Vec::new(), Vec::new(), Vec::new());
    for frame in 0..frames {
        let t = frame as f32 * dt;
        ra.set_material_time(Some(t));
        let mut fa = a.as_frame_scene_with_quality(q);
        fa.post = Some(&post);
        pa = ra.render_offscreen(&device, &fa).expect("A").pixels;
        b.meshes[1].model = translate(-1.0 + speed * t, 0.4, 0.0);
        let mut fb = b.as_frame_scene_with_quality(q);
        fb.post = Some(&post);
        pb = rb.render_offscreen(&device, &fb).expect("B").pixels;
        let mut fc = b.as_frame_scene_with_quality(q);
        fc.post = Some(&post);
        fc.meshes[1].object_key = 0x5EED_0000 + frame as u64;
        pc = rc.render_offscreen(&device, &fc).expect("C").pixels;
    }
    save("velocity_graph", a.width, a.height, &pa);
    save("velocity_moved", a.width, a.height, &pb);
    save("velocity_no_history", a.width, a.height, &pc);
    let (max, over) = compare(&pa, &pb, 3);
    let (_, control) = compare(&pc, &pb, 3);
    eprintln!("velocity: graph vs moved max {max}, {over} px over 3/255; zero-motion control {control} px");
    // Measured on lavapipe: ~44 px ghost (the neighbourhood clamp hides the rest), against an
    // exact match for the graph's motion vectors.
    assert!(
        control > 20,
        "zero motion vectors must ghost the sliding cube ({control} px)"
    );
    assert!(
        over <= 30,
        "the graph's motion vectors must match the moved mesh's ({over} px)"
    );
    // The control must exceed the tolerated error by a margin, or a graph that wrote no motion
    // vectors would match the control within the slack and pass both asserts.
    assert!(
        control >= over + 15,
        "the graph frame is not distinguishable from zero motion: {over} px vs control {control} px"
    );
}

const CHECKER_UV1: &str = r#"(
    format_version: 2,
    textures: [(name: "checker", color_space: Srgb)],
    nodes: {
        "uv": TexCoord1,
        "s": Sample(texture: "checker", uv: "uv"),
        "rgb": Swizzle(input: "s", channels: "rgb"),
    },
    output: (base_color: Some("rgb")),
)"#;

#[test]
fn the_second_uv_set_reaches_texcoord1() {
    let device = device();
    let mut checker = Vec::new();
    for y in 0..8u32 {
        for x in 0..8u32 {
            let on = (x + y) % 2 == 0;
            checker.extend_from_slice(if on {
                &[230, 230, 220, 255]
            } else {
                &[30, 60, 140, 255]
            });
        }
    }
    let mut images = BTreeMap::new();
    images.insert(
        "checker".to_string(),
        LabTexture {
            width: 8,
            height: 8,
            rgba: checker,
        },
    );
    let scaled = CHECKER_UV1.replace(
        "\"uv\": TexCoord1,",
        "\"uv0\": TexCoord, \"uv\": UvTransform(uv: \"uv0\", scale: (3.0, 3.0)),",
    );
    let plain = CHECKER_UV1.replace("TexCoord1", "TexCoord");
    for gpu in [false, true] {
        let mut scene = stage(192, 144);
        scene.materials.push(graph_material(CHECKER_UV1, &images));
        scene.materials.push(graph_material(&scaled, &images));
        scene.materials.push(graph_material(&plain, &images));
        add_cube(&mut scene, 1, translate(0.0, 0.8, 0.0));
        let uv1: Vec<[f32; 2]> = scene.meshes[1]
            .vertices
            .iter()
            .map(|v| [v.uv[0] * 3.0, v.uv[1] * 3.0])
            .collect();
        let render = |scene: &LabScene, uv1: &[[f32; 2]]| {
            let mut frame = scene.as_frame_scene_with_quality(quality(gpu));
            frame.meshes[1].uv1 = uv1;
            FrameRenderer::new()
                .render_offscreen(&device, &frame)
                .expect("frame")
                .pixels
        };
        let with_uv1 = render(&scene, &uv1);
        scene.meshes[1].material = 2;
        let scaled_uv0 = render(&scene, &[]);
        let tag = if gpu { "gpu" } else { "cpu" };
        save(&format!("uv1_{tag}"), scene.width, scene.height, &with_uv1);
        let (max, over) = compare(&with_uv1, &scaled_uv0, 1);
        assert_eq!(over, 0, "{tag}: TexCoord1 must read the uv1 stream (max {max})");
        // A mesh without a second set: TexCoord1 is the first set.
        scene.meshes[1].material = 1;
        let fallback = render(&scene, &[]);
        scene.meshes[1].material = 3;
        let first = render(&scene, &[]);
        let (max, over) = compare(&fallback, &first, 1);
        assert_eq!(over, 0, "{tag}: no uv1 stream reads the first set (max {max})");
    }
}

/// A preview image for a person to look at: written best-effort under the OS temp directory. A
/// preview is not asserted on, so a folder another user owns (a shared `/tmp`) or a read-only temp
/// volume prints a note instead of failing a correct render (audit-v035-flaky M6).
fn write_preview(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    let written = (|| -> Result<(), Box<dyn std::error::Error>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::File::create(path)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(pixels)?;
        writer.finish()?;
        Ok(())
    })();
    if let Err(err) = written {
        eprintln!("preview {} not written: {err}", path.display());
    }
}
