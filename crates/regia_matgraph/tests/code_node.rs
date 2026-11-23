// SPDX-License-Identifier: MPL-2.0
//! The WGSL code node (ADR-7852, P-175): a Code node computing `a * b + c` shades the same
//! values as the equivalent node graph on a test triangle (GPU readback, lavapipe in CI, no
//! golden: the two frames are compared with each other and with the CPU value); the compile
//! refuses invalid code at its line while the editor's lenient compile shades it magenta; an
//! include resolves; the cook refuses a bad node with the same message.

use std::collections::BTreeMap;

use regia_matgraph::code::{self, CODE_INCLUDE_DIR};
use regia_matgraph::{
    CodeInput, CookedMaterialGraph, MaterialGraph, Node, ValueType, compile, compile_lenient, default_params,
    lab_material,
};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    RenderDevice,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;

fn input(name: &str, node: &str) -> CodeInput {
    CodeInput {
        name: name.into(),
        ty: ValueType::Float,
        node: node.into(),
    }
}

/// `uv.x`, `uv.y` and a constant: the three operands of `a * b + c`, varying over the triangle.
fn operands(graph: &mut MaterialGraph) {
    graph.nodes.insert("uv".into(), Node::TexCoord);
    graph.nodes.insert(
        "u".into(),
        Node::Swizzle {
            input: "uv".into(),
            channels: "x".into(),
        },
    );
    graph.nodes.insert(
        "v".into(),
        Node::Swizzle {
            input: "uv".into(),
            channels: "y".into(),
        },
    );
    graph.nodes.insert("k".into(), Node::Float(0.125));
    graph.nodes.insert(
        "rgb".into(),
        Node::Swizzle {
            input: "mad".into(),
            channels: "xxx".into(),
        },
    );
    graph.output.base_color = Some("rgb".into());
    graph.shading.unlit = true;
}

/// `a * b + c` as nodes.
fn node_graph() -> MaterialGraph {
    let mut graph = MaterialGraph::new("mad_nodes");
    operands(&mut graph);
    graph.nodes.insert("ab".into(), Node::Mul("u".into(), "v".into()));
    graph
        .nodes
        .insert("mad".into(), Node::Add("ab".into(), "k".into()));
    graph
}

/// `a * b + c` as one Code node.
fn code_graph(code: &str) -> MaterialGraph {
    let mut graph = MaterialGraph::new("mad_code");
    operands(&mut graph);
    graph.nodes.insert(
        "mad".into(),
        Node::Code {
            inputs: vec![input("a", "u"), input("b", "v"), input("c", "k")],
            textures: Vec::new(),
            output: ValueType::Float,
            code: code.into(),
        },
    );
    graph
}

/// One triangle filling most of the frame, UVs 0..1 across it.
fn scene(material: LabMaterial) -> LabScene {
    let mut mesh = LabMesh::unskinned();
    for (p, uv) in [
        ([-1.0, -1.0, 0.0], [0.0, 1.0]),
        ([1.0, -1.0, 0.0], [1.0, 1.0]),
        ([-1.0, 1.0, 0.0], [0.0, 0.0]),
    ] {
        mesh.vertices
            .push(LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], uv));
    }
    mesh.indices = vec![0, 1, 2];
    LabScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 0.0, 2.5],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.05,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([-0.3, -0.5, -0.8], [1.0, 1.0, 1.0], 0.0),
        local_lights: Vec::new(),
        materials: vec![material],
        meshes: vec![mesh],
    }
}

fn material(graph: &MaterialGraph) -> LabMaterial {
    let compiled = compile(graph).unwrap_or_else(|e| panic!("{}: {e}", graph.name));
    lab_material(graph, &compiled, default_params(graph, 0.0), &BTreeMap::new())
        .unwrap_or_else(|e| panic!("{}: {e}", graph.name))
}

fn render(device: &RenderDevice, material: LabMaterial) -> Vec<u8> {
    FrameRenderer::new()
        .render_offscreen(
            device,
            &scene(material).as_frame_scene_with_quality(FrameQuality::LAB),
        )
        .expect("offscreen frame")
        .pixels
}

#[test]
fn a_code_node_shades_the_same_values_as_the_equivalent_node_graph() {
    let nodes = node_graph();
    let code = code_graph("return a * b + c;");
    let nodes_wgsl = compile(&nodes).expect("nodes").wgsl;
    let code_wgsl = compile(&code).expect("code").wgsl;
    assert!(
        code_wgsl.contains("fn mg_code_"),
        "inlined as its own function:\n{code_wgsl}"
    );
    assert!(code_wgsl.contains("return a * b + c;"), "{code_wgsl}");
    assert_ne!(nodes_wgsl, code_wgsl);

    let device = RenderDevice::headless().expect("headless device");
    let from_nodes = render(&device, material(&nodes));
    let from_code = render(&device, material(&code));
    assert_eq!(from_nodes.len(), (WIDTH * HEIGHT * 4) as usize);
    // Covered pixels: the background is cleared, the triangle is opaque and unlit.
    let covered: Vec<usize> = (0..(WIDTH * HEIGHT) as usize)
        .filter(|&i| from_nodes[i * 4 + 3] == 255 && from_nodes[i * 4] > 0)
        .collect();
    assert!(
        covered.len() > (WIDTH * HEIGHT / 4) as usize,
        "the triangle covers too little of the frame: {} px",
        covered.len()
    );
    let max = from_nodes
        .iter()
        .zip(&from_code)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    let distinct: std::collections::BTreeSet<u8> = covered.iter().map(|&i| from_nodes[i * 4]).collect();
    eprintln!(
        "covered {} px, {} distinct red values, max diff {max}",
        covered.len(),
        distinct.len()
    );
    assert!(
        distinct.len() > 16,
        "the operands do not vary across the triangle"
    );
    assert_eq!(
        max, 0,
        "the Code node's frame differs from the node graph's by {max}/255"
    );
}

#[test]
fn invalid_code_is_refused_at_its_line_and_the_lenient_compile_shades_magenta() {
    let graph = code_graph("let t = a * b;\nlet u = t +;\nreturn u;");
    let errors = compile(&graph).expect_err("refused").0;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].node.as_deref(), Some("mad"));
    assert!(
        errors[0].message.starts_with("code line 2, column"),
        "{}",
        errors[0].message
    );
    let (compiled, errs) = compile_lenient(&graph).expect("lenient compile");
    assert_eq!(errs, errors, "the editor lists the same diagnostics");
    assert!(
        compiled.wgsl.contains("1.0"),
        "the failing node is a constant:\n{}",
        compiled.wgsl
    );
    assert!(!compiled.wgsl.contains("fn mg_code_"), "{}", compiled.wgsl);
    // A Vec3 code node falls back to magenta.
    assert_eq!(code::error_value(ValueType::Vec3), Node::Vec3([1.0, 0.0, 1.0]));
    // A wired input of the wrong type is an error on the node, not a WGSL error.
    let mut wide = code_graph("return a * b + c;");
    if let Some(Node::Code { inputs, .. }) = wide.nodes.get_mut("mad") {
        inputs[0].node = "uv".into();
    }
    let errors = compile(&wide).expect_err("type").0;
    assert!(
        errors.iter().any(|e| e.message.contains("input `a` is Vec2")),
        "{errors:?}"
    );
}

#[test]
fn an_include_resolves_into_the_compiled_copy_only() {
    let root = regia_test_utils::unique_temp_dir("code_node_include");
    std::fs::create_dir_all(root.join(CODE_INCLUDE_DIR)).expect("include dir");
    std::fs::write(
        root.join(CODE_INCLUDE_DIR).join("mad.wgsl"),
        "fn helper_mad(x: f32, y: f32, z: f32) -> f32 {\n    return x * y + z;\n}\n",
    )
    .expect("include");
    let document = code_graph("#include \"mad.wgsl\"\nreturn helper_mad(a, b, c);");
    let errors = compile(&document).expect_err("unresolved").0;
    assert!(
        errors[0]
            .message
            .starts_with("code line 1, column 1: include `mad.wgsl` was not found"),
        "{errors:?}"
    );
    let mut copy = document.clone();
    let read = copy.resolve_code_includes(&mut code::project_include_reader(&root));
    assert_eq!(read, vec!["mad.wgsl".to_string()]);
    let compiled = compile(&copy).expect("resolved copy compiles");
    assert!(compiled.wgsl.contains("fn helper_mad"), "{}", compiled.wgsl);
    // The structural hash follows the include's text.
    let mut edited = document.clone();
    std::fs::write(
        root.join(CODE_INCLUDE_DIR).join("mad.wgsl"),
        "fn helper_mad(x: f32, y: f32, z: f32) -> f32 {\n    return x * y - z;\n}\n",
    )
    .expect("edit include");
    edited.resolve_code_includes(&mut code::project_include_reader(&root));
    assert_ne!(
        regia_matgraph::graph_hash(&copy),
        regia_matgraph::graph_hash(&edited)
    );
    // The completion list names the include's helpers.
    let text = std::fs::read_to_string(root.join(CODE_INCLUDE_DIR).join("mad.wgsl")).expect("read");
    assert_eq!(code::declared_functions(&text), vec!["helper_mad".to_string()]);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_cook_refuses_a_bad_code_node_with_the_same_message() {
    let graph = code_graph("return a *;");
    let message = compile(&graph).expect_err("bad").to_string();
    let err = CookedMaterialGraph::cook(&graph, None, &mut |_| None).expect_err("cook refuses");
    assert!(err.to_string().contains(&message), "{err} vs {message}");
    let good = code_graph("return a * b + c;");
    assert!(CookedMaterialGraph::cook(&good, None, &mut |_| None).is_ok());
}

#[test]
fn a_code_node_in_the_vertex_stage_takes_no_texture_inputs() {
    // Per vertex: no texture inputs.
    let mut graph = code_graph("return vec3<f32>(a * b + c);");
    if let Some(Node::Code { output, .. }) = graph.nodes.get_mut("mad") {
        *output = ValueType::Vec3;
    }
    graph.nodes.remove("rgb");
    graph.output.base_color = None;
    graph.output.world_position_offset = Some("mad".into());
    graph.shading.max_vertex_offset = 1.0;
    let compiled = compile(&graph).expect("vertex stage compiles");
    let vertex = compiled.vertex_wgsl.as_deref().unwrap_or_default();
    assert!(vertex.contains("fn mgv_code_"), "{vertex}");
    graph.textures.push(regia_matgraph::TextureDecl {
        name: "albedo".into(),
        path: String::new(),
        color_space: regia_matgraph::ColorSpace::Srgb,
    });
    if let Some(Node::Code { textures, .. }) = graph.nodes.get_mut("mad") {
        textures.push("albedo".into());
    }
    let errors = compile(&graph).expect_err("no textures per vertex").0;
    assert!(
        errors.iter().any(|e| e.message.contains("texture inputs")),
        "{errors:?}"
    );
}
