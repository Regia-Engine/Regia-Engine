// SPDX-License-Identifier: MPL-2.0
//! Material graphs and the floating origin (spec 78 § 12, ADR-2435).
//!
//! A graph that reads `WorldPosition` (here through `Noise`, a world-space pattern) must shade the
//! same world point the same before and after a rebase. The compiler's output does not depend on the
//! origin; the renderer's composed module adds the origin from the graph's origin slot. The value the
//! graph's `WorldPosition` node reads is evaluated on the CPU the way the composed WGSL computes it
//! (`f32`): the local position plus `material.graph[slot].xyz` of the uploaded block.

use std::collections::BTreeMap;

use regia_matgraph::{
    ColorSpace, MaterialGraph, Node, ParamDecl, ParamValue, TextureDecl, compile, compile_unvalidated,
    default_params, lab_material,
};
use regia_render::surface_graph::{
    GRAPH_PARAM_SLOTS, GRAPH_WORLD_SURFACE_FN, forward_source_with_code, validate_graph_code,
};

/// World-space noise tinted by a parameter: the pattern lives on world points.
const WORLD_NOISE: &str = r#"(
    format_version: 3,
    params: [
        (name: "scale", value: Float(0.25)),
        (name: "tint", value: Vec3((0.8, 0.7, 0.6))),
    ],
    nodes: {
        "wp": WorldPosition,
        "scale": Param("scale"),
        "p": Mul("wp", "scale"),
        "n": Noise(input: "p", octaves: 3),
        "tint": Param("tint"),
        "base": Mul("tint", "n"),
    },
    output: (base_color: Some("base")),
)"#;

/// A graph that reads no position: nothing about it changes.
const UV_ONLY: &str = r#"(
    format_version: 3,
    nodes: {
        "uv": TexCoord,
        "n": Noise(input: "uv", octaves: 2),
        "base": Append("n", "n"),
        "base3": Append("base", "n"),
    },
    output: (base_color: Some("base3")),
)"#;

/// `p = (local + block[slot].xyz) * scale`, as the composed shader computes the `Noise` input.
fn noise_input(
    local: [f32; 3],
    block: &[[f32; 4]; GRAPH_PARAM_SLOTS],
    slot: usize,
    scale_slot: usize,
) -> [f32; 3] {
    std::array::from_fn(|a| (local[a] + block[slot][a]) * block[scale_slot][0])
}

#[test]
fn a_world_position_graph_reads_the_same_world_point_after_a_ten_kilometre_rebase() {
    let graph = MaterialGraph::from_ron(WORLD_NOISE).expect("graph");
    let compiled = compile(&graph).expect("compile");
    // The compiler emits the plain input: the origin is the renderer's.
    assert!(compiled.wgsl.contains("in.world_position"));
    assert!(!compiled.wgsl.contains("material_surface_world"));
    let slot = compiled
        .origin_slot()
        .expect("a position-reading graph has an origin slot");
    assert_eq!(
        slot,
        GRAPH_PARAM_SLOTS - 1,
        "the highest slot the code does not read"
    );
    let scale_slot = compiled.param("scale").expect("scale").slot;
    assert_ne!(slot, scale_slot);
    assert_ne!(slot, compiled.param("tint").expect("tint").slot);

    // The material the runtime builds names the same slot, and the composed module reads it.
    let material =
        lab_material(&graph, &compiled, default_params(&graph, 0.0), &BTreeMap::new()).expect("material");
    let surface = material.surface_graph.as_deref().expect("graph material");
    assert_eq!(surface.origin_slot(), Some(slot));
    validate_graph_code(&compiled.code()).expect("composes in every host");
    let forward = forward_source_with_code(&compiled.code()).expect("forward");
    assert!(forward.contains(&format!("fn {GRAPH_WORLD_SURFACE_FN}(")));
    assert!(forward.contains(&format!("material.graph[{slot}].xyz")));

    // One world point, before and after a rebase by whole 1024 m cells (10 240 m, 0, 5 120 m).
    let world = [10_123.456f32, 37.25, 5_432.125];
    let shift = [10_240.0f32, 0.0, 5_120.0];
    let local_after: [f32; 3] = std::array::from_fn(|a| world[a] - shift[a]);
    let mut before = surface.params;
    surface.write_origin(&mut before, [0.0; 3]);
    let mut after = surface.params;
    surface.write_origin(&mut after, shift);
    // The parameters are not touched.
    assert_eq!(before[scale_slot], after[scale_slot]);
    assert_eq!(after[scale_slot], surface.params[scale_slot]);
    let seen_before = noise_input(world, &before, slot, scale_slot);
    let seen_after = noise_input(local_after, &after, slot, scale_slot);
    assert_eq!(seen_before, seen_after, "the noise samples the same point");
    // Without the origin the pattern would sample a point 10 km away.
    assert_ne!(noise_input(local_after, &before, slot, scale_slot), seen_before);
}

#[test]
fn a_graph_without_positions_has_no_origin_slot_and_composes_as_before() {
    let graph = MaterialGraph::from_ron(UV_ONLY).expect("graph");
    let compiled = compile(&graph).expect("compile");
    assert_eq!(compiled.origin_slot(), None);
    let forward = forward_source_with_code(&compiled.code()).expect("forward");
    assert!(!forward.contains(GRAPH_WORLD_SURFACE_FN));
    let material =
        lab_material(&graph, &compiled, default_params(&graph, 0.0), &BTreeMap::new()).expect("material");
    let surface = material.surface_graph.as_deref().expect("graph material");
    let mut block = surface.params;
    surface.write_origin(&mut block, [10_240.0, 0.0, 0.0]);
    assert_eq!(block, surface.params, "nothing is written");
}

/// The shipped graphs that read positions take an origin slot that none of their parameters use.
#[test]
fn shipped_position_graphs_keep_their_parameters_out_of_the_origin_slot() {
    for (name, text) in [
        (
            "layered_rock",
            include_str!("../assets/layered_rock.matgraph.ron"),
        ),
        ("hull_panel", include_str!("../assets/hull_panel.matgraph.ron")),
        (
            "scatter_rock",
            include_str!("../assets/scatter_rock.matgraph.ron"),
        ),
    ] {
        let graph = MaterialGraph::from_ron(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let compiled = compile(&graph).unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Some(slot) = compiled.origin_slot() {
            assert!(
                compiled.params.iter().all(|p| p.slot != slot),
                "{name}: a parameter sits in the origin slot {slot}"
            );
            // So the block the runtime uploads holds zero there before the renderer writes it.
            assert_eq!(default_params(&graph, 0.0)[slot], [0.0; 4], "{name}");
        }
    }
}

/// `count` float parameters, each multiplied into `root`, so every parameter is read.
fn chained(count: usize, root: Node) -> MaterialGraph {
    let mut g = MaterialGraph::new("chain");
    g.nodes.insert("root".into(), root);
    let mut prev = "root".to_string();
    for i in 0..count {
        let name = format!("p{i}");
        g.params
            .push(ParamDecl::new(name.clone(), ParamValue::Float(0.5)));
        g.nodes.insert(name.clone(), Node::Param(name.clone()));
        let mul = format!("m{i}");
        g.nodes.insert(mul.clone(), Node::Mul(prev, name));
        prev = mul;
    }
    g.output.base_color = Some(prev);
    g
}

fn origin_error(graph: &MaterialGraph) -> String {
    compile_unvalidated(graph)
        .expect_err("a position read that fills every parameter slot has no origin slot")
        .to_string()
}

/// Fifteen referenced parameters plus a world position used to compile and then shade local
/// positions: the renderer had no free slot for the floating origin. That combination is a
/// compile error. Fourteen parameters keep the highest slot, and fifteen parameters with no
/// position still compile.
#[test]
fn a_position_read_that_fills_every_parameter_slot_is_rejected() {
    let message = origin_error(&chained(15, Node::WorldPosition));
    assert!(
        message.contains("floating origin"),
        "expected the origin-slot error, got {message}"
    );

    let compiled = compile_unvalidated(&chained(14, Node::WorldPosition)).expect("14 parameters fit");
    let slot = compiled
        .origin_slot()
        .expect("one slot stays free for the origin");
    assert_eq!(slot, GRAPH_PARAM_SLOTS - 1);
    assert!(compiled.params.iter().all(|p| p.slot != slot));
    // The composed shader adds `material.graph[slot]` to the local position. The sum is the
    // same world point before and after a 1024 m rebase.
    let world = [1_024.0 + 12.5, 0.0, 0.0];
    let shift = [1_024.0, 0.0, 0.0];
    let local = [world[0] - shift[0], world[1] - shift[1], world[2] - shift[2]];
    let seen = |local: [f32; 3], origin: [f32; 3]| {
        [local[0] + origin[0], local[1] + origin[1], local[2] + origin[2]]
    };
    assert_eq!(seen(world, [0.0; 3]), seen(local, shift));

    let full = compile_unvalidated(&chained(15, Node::Vec3([0.2, 0.3, 0.4])))
        .expect("15 parameters and no position still compile");
    assert_eq!(full.origin_slot(), None);
    assert_eq!(full.param("p14").expect("last parameter").slot, 15);

    // A declared parameter that nothing reads does not occupy its slot in the generated text.
    let mut gap = chained(15, Node::WorldPosition);
    gap.nodes.remove("m14");
    gap.output.base_color = Some("m13".into());
    let gap = compile_unvalidated(&gap).expect("the unread slot stays free");
    assert_eq!(gap.origin_slot(), Some(GRAPH_PARAM_SLOTS - 1));
}

/// Triplanar and a vertex-only object position read positions too, so a full parameter block
/// fails the same way.
#[test]
fn triplanar_and_vertex_positions_need_a_free_origin_slot() {
    let mut tri = MaterialGraph::new("tri");
    tri.textures.push(TextureDecl {
        name: "tex".into(),
        path: String::new(),
        color_space: ColorSpace::Linear,
    });
    for i in 0..15 {
        let name = format!("p{i}");
        tri.params
            .push(ParamDecl::new(name.clone(), ParamValue::Float(1.0)));
        tri.nodes.insert(name.clone(), Node::Param(name));
    }
    tri.nodes.insert(
        "sample".into(),
        Node::Triplanar {
            texture: "tex".into(),
            scale: "p0".into(),
            sharpness: 4.0,
        },
    );
    tri.nodes.insert(
        "rgb".into(),
        Node::Swizzle {
            input: "sample".into(),
            channels: "rgb".into(),
        },
    );
    let mut prev = "rgb".to_string();
    for i in 1..15 {
        let mul = format!("m{i}");
        tri.nodes.insert(mul.clone(), Node::Mul(prev, format!("p{i}")));
        prev = mul;
    }
    tri.output.base_color = Some(prev);
    let message = origin_error(&tri);
    assert!(message.contains("floating origin"), "{message}");

    let mut vertex = chained(15, Node::ObjectPosition);
    let offset = vertex.output.base_color.take().expect("offset");
    vertex.output.world_position_offset = Some(offset);
    vertex.shading.max_vertex_offset = 1.0;
    let message = origin_error(&vertex);
    assert!(message.contains("floating origin"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// End to end on the GPU (lavapipe in CI)
// ---------------------------------------------------------------------------------------------

/// Stripes along world X with a period that one 1024 m cell does not divide (k = 0.7 per meter):
/// a rebase that the material did not see moves them by 0.8 of a period.
const X_STRIPES: &str = r#"(
    format_version: 2,
    params: [(name: "tint", value: Color((0.9, 0.5, 0.1)))],
    nodes: {
        "tint": Param("tint"),
        "wp": WorldPosition,
        "x": Swizzle(input: "wp", channels: "x"),
        "k": Float(0.7),
        "xk": Mul("x", "k"),
        "f": Frac("xk"),
        "base": Mul("tint", "f"),
        "e": Mul("tint", "f"),
    },
    output: (base_color: Some("base"), emissive: Some("e")),
)"#;

/// A ground and a long box with the stripe graph, everything moved by `-shift` (local space after a
/// rebase by `shift`).
fn striped_scene(shift: [f32; 3]) -> regia_render::LabScene {
    use regia_render::{DirectionalLightUniform, LabMaterial, LabMesh, LabScene, LabVertex};
    let at = |p: [f32; 3]| [p[0] - shift[0], p[1] - shift[1], p[2] - shift[2]];
    let quad = |y: f32, half: [f32; 2]| {
        [
            LabVertex::from_pos_normal_uv(at([-half[0], y, -half[1]]), [0.0, 1.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv(at([half[0], y, -half[1]]), [0.0, 1.0, 0.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv(at([half[0], y, half[1]]), [0.0, 1.0, 0.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv(at([-half[0], y, half[1]]), [0.0, 1.0, 0.0], [0.0, 1.0]),
        ]
        .to_vec()
    };
    let graph = MaterialGraph::from_ron(X_STRIPES).expect("graph");
    let compiled = compile(&graph).expect("compile");
    let stripes =
        lab_material(&graph, &compiled, default_params(&graph, 0.0), &BTreeMap::new()).expect("material");
    LabScene {
        width: 192,
        height: 144,
        eye: at([0.0, 6.0, 7.0]),
        target: at([0.0, 0.0, 0.0]),
        fov_y: 0.8,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 15_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.9, -0.2], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.3, 0.3, 0.3, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            stripes,
        ],
        meshes: vec![
            LabMesh {
                vertices: quad(0.0, [12.0, 12.0]),
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: quad(0.5, [5.0, 1.5]),
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 1,
                ..LabMesh::unskinned()
            },
        ],
    }
}

/// The same striped surface rendered before a one-cell rebase (origin zero) and after it (local
/// space moved by -1024 m on X, the renderer's origin at +1024 m) draws the same stripes. Needs the
/// renderer to write the origin slot (`SurfaceGraph::write_origin` from `ensure_material` in
/// `regia_render/src/frame/materials.rs`, ADR-2435; applied at integration, ADR-2451).
#[test]
fn a_world_space_pattern_draws_the_same_after_a_rebase_on_the_gpu() {
    use regia_render::{FrameQuality, FrameRenderer, RenderDevice};
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let render = |shift: [f32; 3]| {
        let scene = striped_scene(shift);
        let mut renderer = FrameRenderer::new();
        renderer.set_origin_offset(shift);
        renderer
            .render_offscreen(&device, &scene.as_frame_scene_with_quality(FrameQuality::LAB))
            .expect("frame")
            .pixels
    };
    let before = render([0.0; 3]);
    let after = render([1024.0, 0.0, 0.0]);
    // Control: the stripes are on screen (the striped box differs from a plain one by a lot).
    let max_channel = before.as_chunks::<4>().0.iter().map(|p| p[0]).max().unwrap_or(0);
    assert!(max_channel > 64, "the stripes are visible ({max_channel})");
    let mut worst = 0u8;
    let mut over = 0usize;
    for (a, b) in before.as_chunks::<4>().0.iter().zip(after.as_chunks::<4>().0) {
        let d = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        worst = worst.max(d);
        if d > 8 {
            over += 1;
        }
    }
    eprintln!("before vs after the rebase: max {worst}/255, {over} px over 8/255");
    // A missed origin moves every stripe by 0.8 of a period: thousands of pixels.
    assert!(
        over < 40,
        "the stripes moved with the rebase ({over} px, max {worst})"
    );
}
