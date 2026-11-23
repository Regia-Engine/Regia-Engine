// SPDX-License-Identifier: MPL-2.0
//! The shipped material graphs compile to the same bytes after the `.matgraph.ron` v3 bump
//! (ADR-2318): the `Noise` node and the editor's pin model are additive, so a graph without
//! `Noise` keeps its generated WGSL and its pipeline key.
//!
//! The pins were taken from this file run against the v034 base's `regia_matgraph/src` (f612a1ada,
//! before the bump; the same API): a moved pin here means a default material's shader changed. The
//! inline-function graph checks both switch values (each permutation is its own code).
//!
//! `hull_panel`, `layered_rock`, and `matgraph.example` moved when the tangent-space normal
//! emitter started multiplying the tangent by `in.facing` (a back face is the front frame
//! negated). `scatter_rock` and `graph_v2` have no tangent-space normal output, so their pins
//! stayed. This is shader text, not a golden image.

use std::collections::BTreeMap;

use regia_matgraph::{MaterialGraph, compile, compile_with};

/// FNV-1a 64 of `bytes`.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The v2 inline-function and switch fixture of `all_nodes.rs` (vertex stage, both helpers).
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

/// `(name, surface WGSL digest, vertex WGSL digest or 0, pipeline key)` of every default graph,
/// from the v034 base.
const PINS: &[(&str, u64, u64, u64)] = &[
    ("hull_panel", 0x5196_3dbb_2067_f543, 0x0, 0x5196_3dbb_2067_f543),
    ("layered_rock", 0x04e8_83e3_34d5_4758, 0x0, 0x04e8_83e3_34d5_4758),
    ("scatter_rock", 0x2c92_9acd_0cbd_e992, 0x0, 0x2c92_9acd_0cbd_e992),
    (
        "matgraph.example",
        0x5196_3dbb_2067_f543,
        0x0,
        0x5196_3dbb_2067_f543,
    ),
    (
        "graph_v2/wear=true",
        0xa329_d9c9_0137_6925,
        0xa6d6_a7b2_7a48_c486,
        0x6587_7e1f_bf04_3aae,
    ),
    (
        "graph_v2/wear=false",
        0xec6e_444c_cc78_4739,
        0xa6d6_a7b2_7a48_c486,
        0xfa06_af36_1320_5ef2,
    ),
];

fn digests() -> Vec<(String, u64, u64, u64)> {
    let files = [
        ("hull_panel", include_str!("../assets/hull_panel.matgraph.ron")),
        (
            "layered_rock",
            include_str!("../assets/layered_rock.matgraph.ron"),
        ),
        (
            "scatter_rock",
            include_str!("../assets/scatter_rock.matgraph.ron"),
        ),
        (
            "matgraph.example",
            include_str!("../../../docs/formats/matgraph.example.ron"),
        ),
    ];
    let mut out = Vec::new();
    for (name, text) in files {
        let graph = MaterialGraph::from_ron(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let c = compile(&graph).unwrap_or_else(|e| panic!("{name}: {e}"));
        out.push((
            name.to_string(),
            fnv(c.wgsl.as_bytes()),
            c.vertex_wgsl.as_deref().map_or(0, |v| fnv(v.as_bytes())),
            c.key,
        ));
    }
    let graph = MaterialGraph::from_ron(GRAPH_V2).unwrap_or_else(|e| panic!("graph_v2: {e}"));
    for wear in [true, false] {
        let switches = BTreeMap::from([("wear".to_string(), wear)]);
        let c = compile_with(&graph, &switches).unwrap_or_else(|e| panic!("graph_v2: {e}"));
        out.push((
            format!("graph_v2/wear={wear}"),
            fnv(c.wgsl.as_bytes()),
            c.vertex_wgsl.as_deref().map_or(0, |v| fnv(v.as_bytes())),
            c.key,
        ));
    }
    out
}

#[test]
fn default_graphs_compile_to_the_same_bytes() {
    let got = digests();
    for (name, surface, vertex, key) in &got {
        println!("    (\"{name}\", 0x{surface:016x}, 0x{vertex:016x}, 0x{key:016x}),");
    }
    let pinned: Vec<(String, u64, u64, u64)> = PINS
        .iter()
        .map(|&(n, s, v, k)| (n.to_string(), s, v, k))
        .collect();
    assert_eq!(got, pinned, "a default graph's generated code moved");
}
