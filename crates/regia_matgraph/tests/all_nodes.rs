// SPDX-License-Identifier: MPL-2.0
//! Every node kind, every output, both normal spaces: the generated WGSL must compose into the
//! forward shader, pass naga validation and `spirv-val` (the ADR-026 gate the engine shaders
//! pass), so no node emitter can produce code the GPU path would reject.

use regia_matgraph::{MaterialGraph, Node, NormalSpace, compile};

const GRAPH: &str = r#"(
    format_version: 1,
    name: "every_node",
    params: [
        (name: "f", value: Float(0.5)),
        (name: "v2", value: Vec2((2.0, 3.0))),
        (name: "v3", value: Vec3((0.1, 0.2, 0.3))),
        (name: "v4", value: Vec4((0.1, 0.2, 0.3, 0.4))),
        (name: "c", value: Color((0.9, 0.5, 0.1))),
    ],
    textures: [
        (name: "a", color_space: Srgb),
        (name: "b", color_space: Linear),
        (name: "h", color_space: Linear),
    ],
    nodes: {
        "uv": TexCoord,
        "vc": VertexColor,
        "wp": WorldPosition,
        "wn": WorldNormal,
        "vd": ViewDirection,
        "dist": CameraDistance,
        "time": Time,
        "f": Param("f"),
        "v2": Param("v2"),
        "v3": Param("v3"),
        "v4": Param("v4"),
        "c": Param("c"),
        "k1": Float(0.25),
        "k2": Vec2((0.5, 0.5)),
        "k3": Vec3((0.0, 0.0, 1.0)),
        "k4": Vec4((1.0, 1.0, 1.0, 1.0)),

        "uv_t": UvTransform(uv: "uv", scale: (2.0, 2.0), offset: (0.1, 0.0)),
        "uv_r": UvTransform(uv: "uv_t", rotation: 0.3),
        "uv_p": Parallax(texture: "h", channel: G, uv: "uv_r", scale: "f", min_steps: 2.0, max_steps: 8.0),
        "s": Sample(texture: "a", uv: "uv_p"),
        "s_lod": Sample(texture: "b", uv: "uv", lod: Some("k1")),
        "tri": Triplanar(texture: "b", scale: "f"),

        "add_bc": Add("v3", "k1"),
        "sub": Sub("k4", "v4"),
        "mul": Mul("v2", "k2"),
        "div": Div("dist", "f"),
        "min": Min("v3", "k1"),
        "max": Max("k1", "c"),
        "pow": Pow("max", "f"),
        "dot": Dot("wn", "vd"),
        "lerp": Lerp(a: "c", b: "v3", t: "dot"),
        "lerp_v": Lerp(a: "c", b: "v3", t: "min"),
        "smooth": Smoothstep(edge0: "k1", edge1: "f", x: "time"),
        "sat": Saturate("smooth"),
        "om": OneMinus("sat"),
        "abs": Abs("wp"),
        "norm": Normalize("abs"),
        "len": Length("mul"),
        "rgb": Swizzle(input: "s", channels: "rgb"),
        "alpha_s": Swizzle(input: "s", channels: "a"),
        "spread": Swizzle(input: "om", channels: "xxx"),
        "app2": Append("om", "len"),
        "app3": Append("app2", "k1"),
        "fres": Fresnel(power: "f"),
        "hb": HeightBlend(a: "rgb", b: "lerp", height_a: "om", height_b: "fres", t: "f"),
        "hb_f": HeightBlend(a: "k1", b: "f", height_a: "om", height_b: "fres", t: "sat", depth: 0.05),

        "n1": UnpackNormal("s_lod"),
        "n2": UnpackNormal("tri"),
        "n2s": NormalStrength(normal: "n2", strength: "f"),
        "rnm": BlendNormals(base: "n1", detail: "n2s", mode: Rnm),
        "udn": BlendNormals(base: "rnm", detail: "k3", mode: Udn),
        "wo": BlendNormals(base: "udn", detail: "n1", mode: Whiteout),

        "base_a": Mul("hb", "spread"),
        "base": Add("base_a", "lerp_v"),
        "emissive_a": Mul("app3", "pow"),
        "emissive": Mul("emissive_a", "norm"),
        "occl": Mul("hb_f", "alpha_s"),
        "rough": Mul("div", "sat"),
        "metal": Mul("f", "sub_x"),
        "sub_x": Swizzle(input: "sub", channels: "x"),
    },
    output: (
        base_color: Some("base"),
        alpha: Some("alpha_s"),
        metallic: Some("metal"),
        roughness: Some("rough"),
        normal: Some("wo"),
        occlusion: Some("occl"),
        emissive: Some("emissive"),
    ),
)"#;

fn kinds(graph: &MaterialGraph) -> std::collections::BTreeSet<&'static str> {
    graph.nodes.values().map(Node::kind).collect()
}

#[test]
fn every_node_kind_generates_valid_wgsl_in_both_normal_spaces() {
    let mut graph = MaterialGraph::from_ron(GRAPH).expect("parse");
    // 38 kinds: the fixture must keep covering each one.
    assert_eq!(kinds(&graph).len(), 38, "{:?}", kinds(&graph));
    for space in [NormalSpace::Tangent, NormalSpace::World] {
        graph.output.normal_space = space;
        let compiled = compile(&graph).unwrap_or_else(|e| panic!("{space:?}: {e}"));
        regia_render::validate_surface_graph(&compiled.wgsl)
            .unwrap_or_else(|e| panic!("{space:?}: {e}\n{}", compiled.wgsl));
        assert!(
            compiled.wgsl.contains("material.graph[0].x"),
            "time reads the built-in slot"
        );
        assert!(compiled.wgsl.contains("mg_rnm") && compiled.wgsl.contains("mg_triplanar"));
        // Device-independent forms: no pow of a negative base, no smoothstep with edge0 >= edge1.
        assert!(compiled.wgsl.contains("pow(abs("), "{}", compiled.wgsl);
        assert!(compiled.wgsl.contains("select(step("), "{}", compiled.wgsl);
    }
}

/// The v2 additions: second UV set, object position, trig, static switches, material functions
/// and a vertex stage that samples at an explicit level, uses the triplanar and RNM helpers and
/// reads time. Every host module (forward, instanced, depth, shadow, velocity) must pass naga
/// and `spirv-val`.
const GRAPH_V2: &str = r#"(
    format_version: 2,
    params: [
        (name: "f", value: Float(0.5)),
        (name: "lift", value: Vec3((0.0, 0.1, 0.0))),
    ],
    textures: [
        (name: "a", color_space: Srgb),
        (name: "m", color_space: Linear),
    ],
    switches: [(name: "wear", default: true)],
    functions: {
        "sway": Inline((
            inputs: [(name: "phase", ty: Float), (name: "amp", ty: Vec3)],
            nodes: { "p": Input("phase"), "a": Input("amp"), "s": Sin("p"), "out": Mul("a", "s") },
            outputs: { "offset": "out" },
        )),
        "dim": Inline((
            inputs: [(name: "c", ty: Vec3)],
            nodes: { "c": Input("c"), "k": Float(0.5), "o": Mul("c", "k") },
            outputs: { "o": "o" },
        )),
    },
    nodes: {
        "uv1": TexCoord1,
        "op": ObjectPosition,
        "t": Time,
        "f": Param("f"),
        "lift": Param("lift"),
        "opx": Swizzle(input: "op", channels: "x"),
        "phase": Add("t", "opx"),
        "mask": Sample(texture: "m", uv: "uv1"),
        "mask_r": Swizzle(input: "mask", channels: "r"),
        "amp": Mul("lift", "mask_r"),
        "sway": Call(function: "sway", args: { "phase": "phase", "amp": "amp" }),
        "tri": Triplanar(texture: "m", scale: "f"),
        "tri_r": Swizzle(input: "tri", channels: "r"),
        "n1": UnpackNormal("tri"),
        "rnm": BlendNormals(base: "n1", detail: "n1", mode: Rnm),
        "rnm_y": Swizzle(input: "rnm", channels: "y"),
        "wpo_a": Mul("sway", "tri_r"),
        "wpo": Mul("wpo_a", "rnm_y"),
        "col": Sample(texture: "a", uv: "uv1"),
        "rgb": Swizzle(input: "col", channels: "rgb"),
        "worn": Call(function: "dim", args: { "c": "rgb" }, output: Some("o")),
        "pick": Switch(switch: "wear", on: "worn", off: "rgb"),
        "c": Cos("f"),
        "fr": Frac("c"),
        "base": Mul("pick", "fr"),
    },
    output: (base_color: Some("base"), world_position_offset: Some("wpo")),
    shading: (max_vertex_offset: 0.5),
)"#;

#[test]
fn v2_nodes_switches_functions_and_the_vertex_stage_pass_every_host() {
    use std::collections::BTreeMap;

    let graph = MaterialGraph::from_ron(GRAPH_V2).expect("parse");
    let compiled = compile(&graph).unwrap_or_else(|e| panic!("{e}"));
    let vertex = compiled.vertex_wgsl.as_deref().expect("a vertex stage");
    for needle in [
        "fn material_vertex_offset",
        "textureSampleLevel",
        "mgv_triplanar",
        "mgv_rnm",
        "in.time",
        "sin(",
    ] {
        assert!(vertex.contains(needle), "vertex stage lacks {needle}:\n{vertex}");
    }
    assert!(compiled.wgsl.contains("in.second_uv") && compiled.wgsl.contains("fract("));
    assert!(compiled.uses_uv1 && compiled.uses_time);
    assert!(
        compiled.node_types.contains_key("worn/o"),
        "the switch picks the dimmed branch"
    );
    regia_render::validate_graph_code_spirv(&compiled.code())
        .unwrap_or_else(|e| panic!("{e}\n{}\n{vertex}", compiled.wgsl));

    // The other switch value is another permutation without the function's nodes.
    let mut off = BTreeMap::new();
    off.insert("wear".to_string(), false);
    let plain = regia_matgraph::compile_with(&graph, &off).expect("wear off");
    assert_ne!(plain.key, compiled.key);
    assert!(!plain.node_types.contains_key("worn/o"));
    assert_eq!(
        plain.vertex_wgsl, compiled.vertex_wgsl,
        "the switch only touches the surface"
    );

    // An eye-dependent input cannot feed the offset: shadows have another eye.
    let mut eye = graph.clone();
    eye.nodes.insert("dist".into(), Node::CameraDistance);
    eye.nodes
        .insert("wpo_eye".into(), Node::Mul("wpo".into(), "dist".into()));
    eye.output.world_position_offset = Some("wpo_eye".into());
    let err = compile(&eye).expect_err("eye-dependent offset").to_string();
    assert!(err.contains("dist: CameraDistance is eye-dependent"), "{err}");
}

/// v4: `Noise` on a `Float`, a `Vec2` and a `Vec3`, with one and eight octaves, in the surface
/// and in the vertex stage. Every host module must pass naga and `spirv-val`; the helper is
/// emitted once per stage under its own name, and out-of-range octaves or a `Vec4` input are
/// errors on the node.
const GRAPH_V3: &str = r#"(
    format_version: 4,
    nodes: {
        "uv": TexCoord,
        "wp": WorldPosition,
        "t": Time,
        "n_uv": Noise(input: "uv", octaves: 8),
        "n_wp": Noise(input: "wp"),
        "n_t": Noise(input: "t", octaves: 3),
        "mix": Mul("n_uv", "n_wp"),
        "rough": Add("mix", "n_t"),
        "op": ObjectPosition,
        "sway": Noise(input: "op", octaves: 2),
        "up": Vec3((0.0, 0.05, 0.0)),
        "wpo": Mul("up", "sway"),
    },
    output: (roughness: Some("rough"), base_color: Some("wp"), world_position_offset: Some("wpo")),
    shading: (max_vertex_offset: 0.05),
)"#;

#[test]
fn v3_noise_passes_every_host_in_both_stages() {
    let graph = MaterialGraph::from_ron(GRAPH_V3).expect("parse");
    let compiled = compile(&graph).unwrap_or_else(|e| panic!("{e}"));
    let vertex = compiled.vertex_wgsl.as_deref().expect("a vertex stage");
    assert_eq!(
        compiled.wgsl.matches("fn mg_noise(").count(),
        1,
        "{}",
        compiled.wgsl
    );
    assert!(
        compiled.wgsl.contains("mg_noise(vec3<f32>("),
        "a Vec2 or Float is padded"
    );
    assert!(compiled.wgsl.contains(", 8u)") && compiled.wgsl.contains(", 1u)"));
    assert_eq!(vertex.matches("fn mgv_noise(").count(), 1, "{vertex}");
    assert!(
        !vertex.contains("fn mg_noise("),
        "the stages do not share helper names"
    );
    regia_render::validate_surface_graph(&compiled.wgsl).unwrap_or_else(|e| panic!("{e}\n{}", compiled.wgsl));
    regia_render::validate_graph_code_spirv(&compiled.code())
        .unwrap_or_else(|e| panic!("{e}\n{}\n{vertex}", compiled.wgsl));

    for (bad, want) in [
        (r#"Noise(input: "uv", octaves: 0)"#, "octaves must be 1–8, got 0"),
        (r#"Noise(input: "uv", octaves: 9)"#, "octaves must be 1–8, got 9"),
        (
            r#"Noise(input: "vc")"#,
            "input must be Float, Vec2 or Vec3, got Vec4",
        ),
    ] {
        let text = GRAPH_V3
            .replace(r#"Noise(input: "uv", octaves: 8)"#, bad)
            .replace(r#""uv": TexCoord,"#, r#""uv": TexCoord, "vc": VertexColor,"#);
        let graph = MaterialGraph::from_ron(&text).expect("parse");
        let err = compile(&graph).expect_err(bad).to_string();
        assert!(err.contains(&format!("n_uv: {want}")), "{bad}: {err}");
    }
}
