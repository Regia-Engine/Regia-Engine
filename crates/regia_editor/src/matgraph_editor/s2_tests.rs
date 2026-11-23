// SPDX-License-Identifier: MPL-2.0
//! v0.6.0 Stage 2 acceptance tests of the material graph editor (spec 80 § 12, ADR-9700):
//! preview shapes, the typed inspector, canvas productivity (search, clipboard, arrange,
//! comments and reroutes in layout sidecar v2), per-node previews and the level hookup.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_matgraph::{MaterialGraph, Node};
use regia_render::RenderDevice;
use serde_json::{Value, json};

use super::clipboard::{Arrange, arrange};
use super::inspector::{FieldValue, fields, set_field};
use super::search::{WireFrom, add_wired, search_kinds};
use super::*;
use crate::command::UndoStack;
use crate::material_editor::{PreviewShape, PreviewView, load_preview_mesh, preview_geometry};

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = agent::methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .unwrap_or_else(|| panic!("no method {method}"));
    handler(world, &params)
}

fn ok(world: &mut World, method: &str, params: Value) -> Value {
    call(world, method, params).unwrap_or_else(|e| panic!("{method}: {}", e.message))
}

pub(crate) fn red_graph() -> MaterialGraph {
    MaterialGraph::from_ron(
        r#"(
            format_version: 2,
            params: [(name: "tint", value: Color((0.9, 0.05, 0.03)))],
            nodes: { "tint": Param("tint"), "r": Float(0.5) },
            output: (base_color: Some("tint"), roughness: Some("r")),
        )"#,
    )
    .expect("parses")
}

/// A glTF cube (positions and indices only) in `dir`, as a project mesh.
fn write_cube_gltf(dir: &Path, name: &str) -> PathBuf {
    let (vertices, indices) = crate::material_editor::preview_cube();
    let mut bin = Vec::new();
    for v in &vertices {
        for value in v.position {
            bin.extend(value.to_le_bytes());
        }
    }
    let positions_len = bin.len();
    for index in &indices {
        bin.extend(index.to_le_bytes());
    }
    let bin_name = format!("{name}.bin");
    std::fs::write(dir.join(&bin_name), &bin).expect("bin");
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
"nodes":[{{"mesh":0}}],"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1}}]}}],
"buffers":[{{"uri":"{bin_name}","byteLength":{len}}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{positions_len}}},{{"buffer":0,"byteOffset":{positions_len},"byteLength":{index_len}}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":{count},"type":"VEC3","min":[-{h},-{h},-{h}],"max":[{h},{h},{h}]}},
{{"bufferView":1,"componentType":5125,"count":{index_count},"type":"SCALAR"}}]}}"#,
        len = bin.len(),
        index_len = bin.len() - positions_len,
        count = vertices.len(),
        index_count = indices.len(),
        h = vertices[0].position[0].abs(),
    );
    let path = dir.join(format!("{name}.gltf"));
    std::fs::write(&path, json).expect("gltf");
    path
}

// -- (1) Preview shapes ------------------------------------------------------------------------

/// A red base color reads red with coverage on the cube, the plane and a project mesh
/// (lavapipe or hardware; skips with no adapter). No golden: coverage and hue only.
#[test]
fn a_red_graph_reads_red_on_cube_plane_and_a_project_mesh() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip a_red_graph_reads_red_on_cube_plane_and_a_project_mesh: {err}");
            return;
        }
    };
    let dir = regia_test_utils::unique_temp_dir("matgraph-s2-shapes");
    let mesh = write_cube_gltf(&dir, "box");
    let view = PreviewView::default();
    for shape in [
        PreviewShape::Cube,
        PreviewShape::Plane,
        PreviewShape::Mesh(mesh.display().to_string()),
    ] {
        let geometry = preview_geometry(&shape, None).expect("geometry");
        let image = preview::render_graph_preview_on(
            &device,
            &red_graph(),
            None,
            None,
            [96.0, 96.0],
            &geometry,
            &view,
        )
        .expect("renders");
        // The shape's pixels read red: a red share of the frame (the sky is never red), and the
        // center pixel (the shape) is red.
        let red = |p: &egui::Color32| {
            p.r() > 60 && u16::from(p.r()) > 2 * u16::from(p.g()) && u16::from(p.r()) > 2 * u16::from(p.b())
        };
        let share = image.pixels.iter().filter(|p| red(p)).count() as f32 / image.pixels.len() as f32;
        let stats = preview::measure(&image);
        assert!(share > 0.1, "{shape:?}: red share {share}, {stats:?}");
        let [w, h] = image.size;
        assert!(
            red(&image.pixels[(h / 2) * w + w / 2]),
            "{shape:?} center reads red: {stats:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A project mesh loads its first primitive centered and scaled to the preview's frame
/// radius; over the vertex cap, or missing, it is a clear error.
#[test]
fn a_project_mesh_is_normalized_capped_and_explains_failures() {
    let dir = regia_test_utils::unique_temp_dir("matgraph-s2-mesh");
    let mesh = write_cube_gltf(&dir, "box");
    let geometry = load_preview_mesh(&mesh, 10_000).expect("loads");
    let radius = geometry
        .vertices
        .iter()
        .map(|v| v.position.iter().map(|c| c * c).sum::<f32>().sqrt())
        .fold(0.0f32, f32::max);
    let want = crate::material_editor::PREVIEW_MESH_RADIUS;
    assert!(
        (radius - want).abs() < 1e-3,
        "scaled to the preview frame: {radius}"
    );
    let err = load_preview_mesh(&mesh, 3).expect_err("over the cap");
    assert!(err.contains("vertex") && err.contains('3'), "{err}");
    let err = load_preview_mesh(&dir.join("missing.glb"), 10_000).expect_err("missing");
    assert!(err.contains("missing.glb"), "{err}");
    let err = preview_geometry(&PreviewShape::Mesh("x.fbx".into()), Some(&dir)).expect_err("not glTF");
    assert!(err.contains(".glb") || err.contains("glTF"), "{err}");
    assert_eq!(
        PreviewShape::parse("cube", None).expect("cube"),
        PreviewShape::Cube
    );
    assert!(
        PreviewShape::parse("mesh", None).is_err(),
        "a mesh shape needs a path"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The orbit and light controls move the eye and the light; the default view is the
/// preview's fixed camera.
#[test]
fn the_preview_view_orbits_the_eye_and_turns_the_light() {
    let view = PreviewView::default();
    let eye = view.eye();
    assert!(
        (eye - regia_core::math::Vec3::new(2.4, 1.6, 2.6)).length() < 1e-5,
        "{eye}"
    );
    let turned = PreviewView {
        yaw: std::f32::consts::FRAC_PI_2,
        ..view
    };
    assert!(
        (turned.eye().length() - eye.length()).abs() < 1e-4,
        "orbit keeps the distance"
    );
    assert!((turned.eye() - eye).length() > 1.0);
    let steep = PreviewView { pitch: 10.0, ..view };
    assert!(steep.eye().y < eye.length(), "pitch is clamped short of the pole");
    let lit = PreviewView {
        light_yaw: std::f32::consts::PI,
        ..view
    };
    let (a, b) = (view.light_direction(), lit.light_direction());
    assert!(
        (a[1] - b[1]).abs() < 1e-5 && (a[0] + b[0]).abs() < 1e-5,
        "{a:?} {b:?}"
    );
}

// -- (2) Typed inspector -----------------------------------------------------------------------

/// One kind's row of [`typed_table`]: (kind, reference RON, typed field edits, wires).
type TypedRow = (
    &'static str,
    &'static str,
    Vec<(&'static str, FieldValue)>,
    Vec<(&'static str, &'static str)>,
);

/// Every kind: (reference RON, typed field edits, wires).
fn typed_table() -> Vec<TypedRow> {
    use FieldValue::{Float, Floats, Int, OptText, Text};
    let unit = |k: &'static str| (k, k, Vec::new(), Vec::new());
    let one = |k: &'static str, ron: &'static str| (k, ron, Vec::new(), vec![("x", "x1")]);
    let two = |k: &'static str, ron: &'static str| (k, ron, Vec::new(), vec![("a", "a1"), ("b", "b1")]);
    vec![
        unit("TexCoord"),
        unit("TexCoord1"),
        unit("VertexColor"),
        unit("WorldPosition"),
        unit("WorldNormal"),
        unit("ViewDirection"),
        unit("CameraDistance"),
        unit("ObjectPosition"),
        unit("Time"),
        unit("Wind"),
        (
            "Param",
            r#"Param("tint")"#,
            vec![("param", Text("tint".into()))],
            vec![],
        ),
        ("Float", "Float(0.25)", vec![("value", Float(0.25))], vec![]),
        (
            "Vec2",
            "Vec2((0.5, 0.75))",
            vec![("value", Floats(vec![0.5, 0.75]))],
            vec![],
        ),
        (
            "Vec3",
            "Vec3((0.1, 0.2, 0.3))",
            vec![("value", Floats(vec![0.1, 0.2, 0.3]))],
            vec![],
        ),
        (
            "Vec4",
            "Vec4((0.1, 0.2, 0.3, 0.4))",
            vec![("value", Floats(vec![0.1, 0.2, 0.3, 0.4]))],
            vec![],
        ),
        (
            "Sample",
            r#"Sample(texture: "albedo", uv: "uv", lod: Some("l"))"#,
            vec![("texture", Text("albedo".into()))],
            vec![("uv", "uv"), ("lod", "l")],
        ),
        (
            "Triplanar",
            r#"Triplanar(texture: "albedo", scale: "s", sharpness: 6.0)"#,
            vec![("texture", Text("albedo".into())), ("sharpness", Float(6.0))],
            vec![("scale", "s")],
        ),
        (
            "UvTransform",
            r#"UvTransform(uv: "uv", scale: (2.0, 3.0), offset: (0.5, 0.25), rotation: 0.75)"#,
            vec![
                ("scale", Floats(vec![2.0, 3.0])),
                ("offset", Floats(vec![0.5, 0.25])),
                ("rotation", Float(0.75)),
            ],
            vec![("uv", "uv")],
        ),
        (
            "Parallax",
            r#"Parallax(texture: "h", channel: G, uv: "uv", scale: "s", min_steps: 4.0, max_steps: 12.0)"#,
            vec![
                ("texture", Text("h".into())),
                ("channel", Text("G".into())),
                ("min_steps", Float(4.0)),
                ("max_steps", Float(12.0)),
            ],
            vec![("uv", "uv"), ("scale", "s")],
        ),
        two("Add", r#"Add("a1", "b1")"#),
        two("Sub", r#"Sub("a1", "b1")"#),
        two("Mul", r#"Mul("a1", "b1")"#),
        two("Div", r#"Div("a1", "b1")"#),
        two("Min", r#"Min("a1", "b1")"#),
        two("Max", r#"Max("a1", "b1")"#),
        two("Pow", r#"Pow("a1", "b1")"#),
        two("Dot", r#"Dot("a1", "b1")"#),
        (
            "Lerp",
            r#"Lerp(a: "a1", b: "b1", t: "t1")"#,
            vec![],
            vec![("a", "a1"), ("b", "b1"), ("t", "t1")],
        ),
        (
            "Smoothstep",
            r#"Smoothstep(edge0: "e0", edge1: "e1", x: "x1")"#,
            vec![],
            vec![("edge0", "e0"), ("edge1", "e1"), ("x", "x1")],
        ),
        one("Saturate", r#"Saturate("x1")"#),
        one("OneMinus", r#"OneMinus("x1")"#),
        one("Abs", r#"Abs("x1")"#),
        one("Normalize", r#"Normalize("x1")"#),
        one("Length", r#"Length("x1")"#),
        one("Sin", r#"Sin("x1")"#),
        one("Cos", r#"Cos("x1")"#),
        one("Frac", r#"Frac("x1")"#),
        (
            "Noise",
            r#"Noise(input: "p", octaves: 5)"#,
            vec![("octaves", Int(5))],
            vec![("input", "p")],
        ),
        (
            "Swizzle",
            r#"Swizzle(input: "v", channels: "zyx")"#,
            vec![("channels", Text("zyx".into()))],
            vec![("input", "v")],
        ),
        two("Append", r#"Append("a1", "b1")"#),
        ("Fresnel", r#"Fresnel(power: "k")"#, vec![], vec![("power", "k")]),
        (
            "HeightBlend",
            r#"HeightBlend(a: "a1", b: "b1", height_a: "ha", height_b: "hb", t: "t1", depth: 0.5)"#,
            vec![("depth", Float(0.5))],
            vec![
                ("a", "a1"),
                ("b", "b1"),
                ("height_a", "ha"),
                ("height_b", "hb"),
                ("t", "t1"),
            ],
        ),
        one("UnpackNormal", r#"UnpackNormal("x1")"#),
        (
            "NormalStrength",
            r#"NormalStrength(normal: "n", strength: "s")"#,
            vec![],
            vec![("normal", "n"), ("strength", "s")],
        ),
        (
            "BlendNormals",
            r#"BlendNormals(base: "b", detail: "d", mode: Whiteout)"#,
            vec![("mode", Text("Whiteout".into()))],
            vec![("base", "b"), ("detail", "d")],
        ),
        (
            "Switch",
            r#"Switch(switch: "wet", on: "a1", off: "b1")"#,
            vec![("switch", Text("wet".into()))],
            vec![("on", "a1"), ("off", "b1")],
        ),
        (
            "Call",
            r#"Call(function: "f", args: {"c": "v"}, output: Some("out"))"#,
            vec![
                ("function", Text("f".into())),
                ("output", OptText(Some("out".into()))),
            ],
            vec![("c", "v")],
        ),
        (
            "Input",
            r#"Input("x")"#,
            vec![("input", Text("x".into()))],
            vec![],
        ),
        // The Code node's text and declaration are the code view's (`code_editor`), not typed
        // rows; its wires are its input names (ADR-7852).
        (
            "Code",
            r#"Code(inputs: [(name: "a", ty: Float, node: "x")], output: Float, code: "return a;")"#,
            vec![],
            vec![("a", "x")],
        ),
    ]
}

/// The typed inspector and the RON path give identical nodes for every kind in
/// `Node::KINDS`, and every typed edit is a row the inspector lists.
#[test]
fn the_typed_inspector_and_ron_give_identical_nodes_for_every_kind() {
    let table = typed_table();
    let covered: BTreeSet<&str> = table.iter().map(|(k, ..)| *k).collect();
    let kinds: BTreeSet<&str> = Node::KINDS.iter().copied().collect();
    assert_eq!(covered, kinds, "the table covers every kind");
    for (kind, ron_text, edits, wires) in table {
        let reference: Node = ron::from_str(ron_text).unwrap_or_else(|e| panic!("{kind}: {e}"));
        let mut node = Node::default_of(kind).expect("kind");
        let rows: Vec<&'static str> = fields(&node).iter().map(|f| f.name).collect();
        for (field, value) in edits {
            assert!(rows.contains(&field), "{kind}: no row `{field}` in {rows:?}");
            node = set_field(&node, field, value).unwrap_or_else(|e| panic!("{kind}.{field}: {e}"));
        }
        for (slot, wire) in wires {
            node.set_input(slot, Some(wire.to_string())).expect("wire");
        }
        assert_eq!(node, reference, "{kind}");
        // The rows read back what was set.
        for field in fields(&node) {
            let again = set_field(&node, field.name, field.value.clone()).expect("same value");
            assert_eq!(again, node, "{kind}.{}: a row's own value is a no-op", field.name);
        }
    }
}

/// Typed edits keep values in range: `Noise` octaves 1..8, finite numbers only, enums by name.
#[test]
fn typed_edits_clamp_and_refuse_bad_values() {
    let noise = Node::default_of("Noise").expect("noise");
    let high = set_field(&noise, "octaves", FieldValue::Int(12)).expect("clamps");
    assert!(matches!(high, Node::Noise { octaves: 8, .. }), "{high:?}");
    let low = set_field(&noise, "octaves", FieldValue::Int(0)).expect("clamps");
    assert!(matches!(low, Node::Noise { octaves: 1, .. }), "{low:?}");
    let float = Node::Float(0.0);
    assert!(set_field(&float, "value", FieldValue::Float(f32::NAN)).is_err());
    assert!(set_field(&float, "nope", FieldValue::Float(1.0)).is_err());
    let blend = Node::default_of("BlendNormals").expect("blend");
    assert!(set_field(&blend, "mode", FieldValue::Text("Sideways".into())).is_err());
    let swizzle = Node::default_of("Swizzle").expect("swizzle");
    assert!(set_field(&swizzle, "channels", FieldValue::Text("xq".into())).is_err());
}

// -- (3) Canvas productivity -------------------------------------------------------------------

#[test]
fn fuzzy_search_ranks_kinds_and_filters_by_the_dragged_pin() {
    assert_eq!(search_kinds("lrp", None, false).first(), Some(&"Lerp"));
    assert_eq!(search_kinds("Sample", None, false).first(), Some(&"Sample"));
    assert!(
        !search_kinds("", None, false).contains(&"Input"),
        "Input is for functions"
    );
    assert!(search_kinds("", None, true).contains(&"Input"));
    let from_out = WireFrom::Output("k".into());
    let kinds = search_kinds("", Some(&from_out), false);
    assert!(kinds.contains(&"Mul") && kinds.contains(&"Saturate"), "{kinds:?}");
    assert!(
        !kinds.contains(&"Float") && !kinds.contains(&"TexCoord"),
        "a kind without inputs cannot take the wire: {kinds:?}"
    );
    assert!(search_kinds("zzzz", None, false).is_empty());
}

#[test]
fn search_add_wires_the_new_node_to_the_dragged_pin() {
    let mut doc = MatGraphDoc::default();
    doc.add_node(Some("k"), Node::Float(0.5), None).expect("add");
    doc.add_node(Some("m"), Node::Mul(String::new(), String::new()), None)
        .expect("add");
    let sat = add_wired(
        &mut doc,
        "Saturate",
        [10.0, 0.0],
        Some(&WireFrom::Output("k".into())),
    )
    .expect("add");
    assert_eq!(doc.graph.nodes[&sat], Node::Saturate("k".into()));
    let f = add_wired(
        &mut doc,
        "Float",
        [0.0, 0.0],
        Some(&WireFrom::Input {
            node: "m".into(),
            slot: "b".into(),
        }),
    )
    .expect("add");
    assert_eq!(doc.graph.nodes["m"], Node::Mul(String::new(), f.clone()));
    let v = add_wired(
        &mut doc,
        "Vec3",
        [0.0, 0.0],
        Some(&WireFrom::Surface("base_color".into())),
    )
    .expect("add");
    assert_eq!(doc.graph.output.base_color.as_deref(), Some(v.as_str()));
    assert!(add_wired(&mut doc, "Float", [0.0, 0.0], Some(&WireFrom::Output("k".into()))).is_err());
}

/// The normalized WGSL of a graph (comments aside), as `collapse_selection` compares it.
fn code(graph: &MaterialGraph) -> String {
    super::normalized_code(graph, None, None).expect("compiles")
}

/// Pasting a 3-node selection into a second tab compiles to the same normalized WGSL as the
/// source sub-graph; pasting into the same tab renames the clashing names and the copies'
/// references follow; references outside the selection are left unwired.
#[test]
fn a_pasted_selection_compiles_to_the_same_code_in_a_second_tab() {
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    let mut editor = MatGraphEditor::default();
    editor.new_graph(None);
    world.insert_resource(editor);
    edit(&mut world, "fixture", None, |d| {
        d.add_node(Some("c"), Node::Vec3([0.8, 0.2, 0.1]), Some([0.0, 0.0]))?;
        d.add_node(Some("k"), Node::Float(0.5), Some([0.0, 120.0]))?;
        d.add_node(Some("m"), Node::Mul("c".into(), "k".into()), Some([230.0, 60.0]))?;
        d.set_output("base_color", Some("m"))
    })
    .expect("fixture");
    let source = world.resource::<MatGraphEditor>().doc.graph.clone();
    let picked: BTreeSet<String> = ["c", "k", "m"].map(String::from).into();
    let text = copy_selection(&mut world, &picked).expect("copy");
    assert!(text.contains("Mul"), "the clipboard is RON of the nodes: {text}");

    // Same tab: clashing names are renamed and the copy reads the copies.
    let renamed = paste(&mut world, None, [500.0, 0.0]).expect("paste");
    assert_eq!(renamed.len(), 3);
    let doc = world.resource::<MatGraphEditor>().doc.clone();
    let m2 = &renamed["m"];
    assert_ne!(m2, "m");
    assert_eq!(
        doc.graph.nodes[m2],
        Node::Mul(renamed["c"].clone(), renamed["k"].clone())
    );
    assert_eq!(doc.layout[m2], [730.0, 60.0], "relative positions survive");

    // A second tab: the pasted sub-graph compiles to the source's code.
    world.resource_mut::<MatGraphEditor>().open_tab();
    world.resource_mut::<MatGraphEditor>().new_graph(None);
    let names = paste(&mut world, Some(&text), [0.0, 0.0]).expect("paste");
    let m = names["m"].clone();
    edit(&mut world, "out", None, move |d| {
        d.set_output("base_color", Some(&m))
    })
    .expect("out");
    let pasted = world.resource::<MatGraphEditor>().doc.graph.clone();
    assert_eq!(code(&pasted), code(&source));

    // A reference outside the selection is unwired in the copy.
    let lone: BTreeSet<String> = ["m"].map(String::from).into();
    let text = super::clipboard::copy_nodes(&world.resource::<MatGraphEditor>().doc, &lone).expect("copy");
    let mut doc = MatGraphDoc::default();
    let names = super::clipboard::paste_nodes(&mut doc, &text, [0.0, 0.0]).expect("paste");
    assert_eq!(
        doc.graph.nodes[&names["m"]],
        Node::Mul(String::new(), String::new())
    );
    assert!(super::clipboard::paste_nodes(&mut doc, "not a clip", [0.0, 0.0]).is_err());
}

#[test]
fn align_and_distribute_move_the_picked_nodes() {
    let mut doc = MatGraphDoc::default();
    for (name, pos) in [("a", [0.0, 0.0]), ("b", [50.0, 30.0]), ("c", [400.0, 90.0])] {
        doc.add_node(Some(name), Node::Time, Some(pos)).expect("add");
    }
    let all: BTreeSet<String> = ["a", "b", "c"].map(String::from).into();
    let mut left = doc.clone();
    arrange(&mut left, &all, Arrange::AlignLeft).expect("align");
    assert!(left.layout.values().all(|p| p[0] == 0.0), "{:?}", left.layout);
    let mut top = doc.clone();
    arrange(&mut top, &all, Arrange::AlignTop).expect("align");
    assert!(top.layout.values().all(|p| p[1] == 0.0));
    let mut spread = doc.clone();
    arrange(&mut spread, &all, Arrange::DistributeX).expect("distribute");
    assert_eq!(spread.layout["b"][0], 200.0, "evenly between the ends");
    assert_eq!(spread.layout["b"][1], 30.0, "the other axis stays");
    let two: BTreeSet<String> = ["a", "b"].map(String::from).into();
    assert!(
        arrange(&mut doc, &two, Arrange::DistributeY).is_err(),
        "needs three"
    );
    assert_eq!(
        Arrange::parse("align_center_x").expect("op"),
        Arrange::AlignCenterX
    );
}

/// A v1 layout loads under v2 (no comments, no reroutes); a v2 layout with both round-trips
/// through save and open byte for byte; a newer one is ignored.
#[test]
fn a_v1_layout_loads_under_v2_and_v2_round_trips() {
    let dir = regia_test_utils::unique_temp_dir("matgraph-s2-layout");
    let path = dir.join("g.matgraph.ron");
    std::fs::write(&path, red_graph().to_ron().expect("ron")).expect("graph");
    std::fs::write(
        layout_path(&path),
        "(format_version: 1, positions: {\"tint\": (1.0, 2.0)})",
    )
    .expect("v1");
    let v1 = read_layout_file(&path);
    assert_eq!(v1.positions["tint"], [1.0, 2.0]);
    assert!(v1.comments.is_empty() && v1.reroutes.is_empty());

    let mut editor = MatGraphEditor::default();
    editor.open(&path).expect("open");
    assert_eq!(editor.doc.layout["tint"], [1.0, 2.0]);
    editor
        .doc
        .add_comment("base color", [-10.0, -10.0], [300.0, 200.0])
        .expect("comment");
    editor
        .doc
        .set_reroute("r", "x", vec![[100.0, 50.0], [150.0, 80.0]])
        .expect_err("r has no input x");
    editor
        .doc
        .graph
        .nodes
        .insert("s".into(), Node::Saturate("r".into()));
    editor
        .doc
        .set_reroute("s", "x", vec![[100.0, 50.0], [150.0, 80.0]])
        .expect("reroute");
    editor.revision += 1;
    editor.save(None).expect("save");
    let bytes = std::fs::read(layout_path(&path)).expect("layout");
    let v2 = read_layout_file(&path);
    assert_eq!(v2.format_version, LAYOUT_FORMAT_VERSION);
    assert_eq!(v2.comments.len(), 1);
    assert_eq!(v2.comments[0].text, "base color");
    assert_eq!(v2.reroutes[0].points, vec![[100.0, 50.0], [150.0, 80.0]]);

    let mut again = MatGraphEditor::default();
    again.open(&path).expect("reopen");
    assert_eq!(again.doc.notes, editor.doc.notes);
    again.save(None).expect("save again");
    assert_eq!(
        std::fs::read(layout_path(&path)).expect("layout"),
        bytes,
        "byte for byte"
    );

    std::fs::write(
        layout_path(&path),
        format!(
            "(format_version: {}, positions: {{\"a\": (1.0, 2.0)}})",
            LAYOUT_FORMAT_VERSION + 1
        ),
    )
    .expect("newer");
    assert!(
        read_layout_file(&path).positions.is_empty(),
        "a newer layout is ignored"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// -- (4) Per-node previews ---------------------------------------------------------------------

#[test]
fn a_node_preview_routes_any_width_to_an_unlit_base_color() {
    let graph = MaterialGraph::from_ron(
        r#"(
            format_version: 4,
            nodes: {
                "f": Float(0.5), "v2": Vec2((0.1, 0.9)), "v3": Vec3((0.2, 0.4, 0.6)),
                "v4": Vec4((0.3, 0.3, 0.3, 1.0)), "uv": TexCoord, "n": Noise(input: "uv", octaves: 3),
            },
            output: (base_color: Some("v3"), roughness: Some("f")),
        )"#,
    )
    .expect("parses");
    for node in ["f", "v2", "v3", "v4", "n"] {
        let derived = node_preview::derived_graph(&graph, node).unwrap_or_else(|e| panic!("{node}: {e}"));
        assert!(derived.shading.unlit, "{node}");
        assert!(derived.output.base_color.is_some() && derived.output.roughness.is_none());
        regia_matgraph::compile_unvalidated(&derived).unwrap_or_else(|e| panic!("{node}: {e}"));
    }
    assert!(node_preview::derived_graph(&graph, "missing").is_err());
}

/// A `Noise` node's swatch has many gray levels, a constant's a few (lavapipe or hardware;
/// skips with no adapter).
#[test]
fn a_noise_node_preview_varies_and_a_constant_is_flat() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip a_noise_node_preview_varies_and_a_constant_is_flat: {err}");
            return;
        }
    };
    let graph = MaterialGraph::from_ron(
        r#"(
            format_version: 4,
            nodes: {
                "wp": WorldPosition, "k": Float(4.0), "p": Mul("wp", "k"),
                "n": Noise(input: "p", octaves: 4), "c": Float(0.5),
            },
            output: (base_color: Some("c")),
        )"#,
    )
    .expect("parses");
    let levels = |node: &str| {
        let swatch =
            node_preview::render_node_preview(&device, &graph, node, None, None, 96).expect("renders");
        node_preview::gray_levels(&swatch.image)
    };
    let (flat, noisy) = (levels("c"), levels("n"));
    assert!(flat <= 4, "a constant is flat: {flat}");
    assert!(noisy >= 16, "noise varies: {noisy} (flat {flat})");
}

#[test]
fn node_previews_are_cached_by_pipeline_key_and_capped_per_frame() {
    let mut cache = node_preview::NodePreviewCache::default();
    let graph = MaterialGraph::from_ron(
        r#"(format_version: 4, nodes: { "a": Float(0.5), "b": Float(0.5), "c": Float(0.2), "d": Time, "e": TexCoord }, output: ())"#,
    )
    .expect("parses");
    // Without a device nothing renders, but the plan says which nodes would.
    let plan = cache.plan(&graph, 7, node_preview::NODE_PREVIEWS_PER_FRAME);
    assert!(plan.len() <= node_preview::NODE_PREVIEWS_PER_FRAME);
    assert!(!plan.is_empty());
    let keys: BTreeSet<u64> = plan.iter().map(|(_, k)| *k).collect();
    assert!(keys.len() <= plan.len());
    assert!(cache.plan(&graph, 7, 0).is_empty(), "a zero cap renders nothing");
}

/// The swatch cache is bounded (ADR-7451 (2)): 500 distinct keys keep it at or under
/// `NODE_PREVIEW_SWATCH_CAP`, least recently used first out, and a swatch drawn every frame
/// stays.
#[test]
fn node_swatches_are_capped_and_a_swatch_drawn_every_frame_stays() {
    let ctx = egui::Context::default();
    let texture = |name: &str| {
        ctx.load_texture(
            name,
            egui::ColorImage::filled([1, 1], egui::Color32::WHITE),
            egui::TextureOptions::LINEAR,
        )
    };
    let mut cache = node_preview::NodePreviewCache::default();
    // Keys in no particular order (the eviction goes by use, not by key); the hot one is the
    // smallest.
    let key = |i: u64| (i + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    cache.settle("hot", 1, Ok(0), Some(texture("hot")));
    let cap = node_preview::NODE_PREVIEW_SWATCH_CAP;
    for i in 0..500u64 {
        // One frame: the canvas draws the hot node, then one new swatch lands.
        assert!(
            cache.swatch("hot").is_some(),
            "the hot swatch was evicted at frame {i}"
        );
        cache.settle(&format!("n{i}"), 1, Ok(key(i)), Some(texture(&format!("n{i}"))));
        assert!(
            cache.swatch_count() <= cap,
            "frame {i}: {} swatches",
            cache.swatch_count()
        );
    }
    assert!(cache.swatch("hot").is_some());
    assert_eq!(cache.swatch_count(), cap, "the cache fills up to its cap");
    // The least recently used cold swatches went first: the newest cap - 1 stay with the hot one.
    let kept: Vec<u64> = (0..500)
        .filter(|i| cache.swatch(&format!("n{i}")).is_some())
        .collect();
    assert_eq!(kept, ((500 - (cap as u64 - 1))..500).collect::<Vec<_>>());
    cache.clear();
    assert_eq!(cache.swatch_count(), 0);
}

// -- (5) Level hookup and the content browser --------------------------------------------------

pub(crate) fn level_world(root: &Path) -> (World, Entity) {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    crate::mesh_source::register_level_types(app.world_mut());
    let mut world = std::mem::take(app.world_mut());
    world.insert_resource(UndoStack::new());
    let owner = world
        .spawn(regia_render::MeshSource::new("assets/box.gltf", ""))
        .id();
    let mut selection = crate::selection::Selection::default();
    selection.select(owner, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
    (world, owner)
}

/// Apply to selection sets the selected mesh owners' `MeshSource.material` to the open graph
/// (project-relative) as one undo step, the content-browser drop's edit.
#[test]
fn apply_to_selection_assigns_the_open_graph_as_one_undo_step() {
    let root = regia_test_utils::unique_temp_dir("matgraph-s2-apply");
    std::fs::create_dir_all(root.join("assets/mat")).expect("assets");
    let (mut world, owner) = level_world(&root);
    let graph_path = root.join("assets/mat/red.matgraph.ron");
    std::fs::write(&graph_path, red_graph().to_ron().expect("ron")).expect("graph");
    let mut editor = MatGraphEditor {
        root: Some(root.clone()),
        ..Default::default()
    };
    editor.open(&graph_path).expect("open");
    world.insert_resource(editor);
    let depth = world.resource::<UndoStack>().undo_depth();
    let report = apply_to_selection(&mut world).expect("applies");
    assert_eq!(report.path, "assets/mat/red.matgraph.ron");
    assert_eq!(report.owners, 1);
    let material = |world: &World| {
        world
            .get::<regia_render::MeshSource>(owner)
            .map(|m| m.material.clone())
            .unwrap_or_default()
    };
    assert_eq!(material(&world), "assets/mat/red.matgraph.ron");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 1);
    crate::command::undo_world(&mut world);
    assert_eq!(material(&world), "");

    // Nothing selected, or an unsaved graph, is a clear refusal.
    world.resource_mut::<crate::selection::Selection>().clear();
    assert!(
        apply_to_selection(&mut world)
            .expect_err("no selection")
            .contains("select")
    );
    world.resource_mut::<MatGraphEditor>().new_graph(None);
    assert!(
        apply_to_selection(&mut world)
            .expect_err("unsaved")
            .contains("save")
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Apply to selection is all or nothing (ADR-7451 (1)): when one owner's capture fails, no owner
/// changes and no undo entry is pushed; a clean apply over two owners is exactly one undo step.
#[test]
fn a_material_apply_whose_second_capture_fails_changes_no_owner() {
    let root = regia_test_utils::unique_temp_dir("matgraph-s2-apply-atomic");
    let (mut world, first) = level_world(&root);
    // The second owner has no `MeshSource`: its capture fails after the first one's.
    let bare = world.spawn_empty().id();
    let material = |world: &World, e: Entity| {
        world
            .get::<regia_render::MeshSource>(e)
            .map(|m| m.material.clone())
    };
    let depth = world.resource::<UndoStack>().undo_depth();
    let err = assign_material(&mut world, &[first, bare], "assets/mat/red.matgraph.ron")
        .expect_err("the second owner has no MeshSource");
    assert!(err.contains("cannot assign the material"), "{err}");
    assert_eq!(
        material(&world, first),
        Some(String::new()),
        "the first owner is unchanged"
    );
    assert_eq!(material(&world, bare), None);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth, "no undo entry");

    // Two good owners: one undo step that sets both and takes both back.
    let second = world
        .spawn(regia_render::MeshSource::new("assets/box.gltf", ""))
        .id();
    assign_material(&mut world, &[first, second], "assets/mat/red.matgraph.ron").expect("applies");
    for e in [first, second] {
        assert_eq!(
            material(&world, e).as_deref(),
            Some("assets/mat/red.matgraph.ron")
        );
    }
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 1);
    crate::command::undo_world(&mut world);
    for e in [first, second] {
        assert_eq!(material(&world, e), Some(String::new()));
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_content_browser_creates_graphs_and_functions() {
    use crate::content::{NewAssetKind, create_asset};
    let root = regia_test_utils::unique_temp_dir("matgraph-s2-create");
    let graph = create_asset(&root, NewAssetKind::MaterialGraph, "assets/m/new.matgraph.ron").expect("graph");
    let text = std::fs::read_to_string(root.join(&graph)).expect("read");
    let parsed = MaterialGraph::from_ron(&text).expect("a valid graph");
    assert_eq!(parsed.name, "new");
    let function =
        create_asset(&root, NewAssetKind::MaterialFunction, "assets/m/f.matfunc.ron").expect("function");
    let text = std::fs::read_to_string(root.join(&function)).expect("read");
    regia_matgraph::MaterialFunction::from_ron(&text).expect("a valid function");
    assert!(create_asset(&root, NewAssetKind::MaterialGraph, "assets/m/x.material.ron").is_err());
    assert_eq!(
        NewAssetKind::parse("material_graph").expect("kind"),
        NewAssetKind::MaterialGraph
    );
    assert_eq!(
        NewAssetKind::parse("material_function").expect("kind"),
        NewAssetKind::MaterialFunction
    );
    let _ = std::fs::remove_dir_all(&root);
}

// -- Undo for every new edit kind --------------------------------------------------------------

/// The graph's RON and its layout sidecar's RON, as save would write them.
fn bytes(world: &World) -> (String, String) {
    let doc = &world.resource::<MatGraphEditor>().doc;
    (
        doc.graph.to_ron().expect("graph"),
        ron::ser::to_string_pretty(&layout_file_of(doc), regia_core::ron_pretty()).expect("layout"),
    )
}

/// Undo restores the graph and layout bytes after each new edit kind: paste, cut, a typed
/// field, align, a comment, a reroute and a search-add.
#[test]
fn undo_restores_graph_and_layout_bytes_for_each_new_edit_kind() {
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    let mut editor = MatGraphEditor::default();
    editor.new_graph(None);
    world.insert_resource(editor);
    edit(&mut world, "fixture", None, |d| {
        d.add_node(Some("c"), Node::Vec3([0.8, 0.2, 0.1]), Some([0.0, 0.0]))?;
        d.add_node(Some("k"), Node::Float(0.5), Some([0.0, 120.0]))?;
        d.add_node(Some("m"), Node::Mul("c".into(), "k".into()), Some([230.0, 60.0]))?;
        d.set_output("base_color", Some("m"))
    })
    .expect("fixture");
    let picked: BTreeSet<String> = ["c", "k"].map(String::from).into();
    copy_selection(&mut world, &picked).expect("copy");
    type Step = Box<dyn Fn(&mut World) -> Result<(), String>>;
    let picked2 = picked.clone();
    let steps: Vec<(&str, Step)> = vec![
        ("paste", Box::new(|w| paste(w, None, [400.0, 0.0]).map(|_| ()))),
        ("cut", Box::new(move |w| cut_selection(w, &picked2).map(|_| ()))),
        (
            "typed field",
            Box::new(|w| set_node_field(w, "k", "value", FieldValue::Float(0.75))),
        ),
        (
            "align",
            Box::new(move |w| arrange_nodes(w, &picked, Arrange::AlignTop)),
        ),
        (
            "comment",
            Box::new(|w| {
                edit(w, "comment", None, |d| {
                    d.add_comment("note", [0.0, 0.0], [10.0, 10.0]).map(|_| ())
                })
            }),
        ),
        (
            "reroute",
            Box::new(|w| edit(w, "reroute", None, |d| d.set_reroute("m", "a", vec![[5.0, 5.0]]))),
        ),
        (
            "search add",
            Box::new(|w| {
                search_add(w, "Saturate", [0.0, 300.0], Some(WireFrom::Output("k".into()))).map(|_| ())
            }),
        ),
    ];
    for (label, step) in steps {
        let before = bytes(&world);
        let depth = world.resource::<UndoStack>().undo_depth();
        step(&mut world).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_ne!(bytes(&world), before, "{label} changed something");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            depth + 1,
            "{label} is one step"
        );
        crate::command::undo_world(&mut world);
        assert_eq!(bytes(&world), before, "undo of {label} restores the bytes");
    }
}

// -- Agent surface ------------------------------------------------------------------------------

/// The Stage 2 methods over the protocol: typed fields, search-add, clipboard, arrange,
/// annotate, preview shapes and a node preview's refusal for an unknown node.
#[test]
fn stage2_methods_work_over_the_protocol() {
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    ok(&mut world, "editor.matgraph_open", json!({ "create": true }));
    ok(
        &mut world,
        "editor.matgraph_edit",
        json!({ "op": "add", "name": "k", "node": "Float(0.5)" }),
    );
    let rows = ok(&mut world, "editor.matgraph_node_fields", json!({ "name": "k" }));
    assert_eq!(rows["fields"][0]["name"], "value");
    ok(
        &mut world,
        "editor.matgraph_node_set",
        json!({ "name": "k", "field": "value", "value": 0.25 }),
    );
    assert_eq!(
        world.resource::<MatGraphEditor>().doc.graph.nodes["k"],
        Node::Float(0.25)
    );
    let found = ok(
        &mut world,
        "editor.matgraph_search",
        json!({ "query": "sat", "from": "k" }),
    );
    assert_eq!(found["kinds"][0], "Saturate");
    let added = ok(
        &mut world,
        "editor.matgraph_search",
        json!({ "query": "sat", "from": "k", "add": "Saturate", "pos": [100.0, 0.0] }),
    );
    let sat = added["added"].as_str().expect("name").to_string();
    assert_eq!(
        world.resource::<MatGraphEditor>().doc.graph.nodes[&sat],
        Node::Saturate("k".into())
    );
    let copied = ok(
        &mut world,
        "editor.matgraph_clipboard",
        json!({ "op": "copy", "nodes": ["k"] }),
    );
    assert!(copied["text"].as_str().is_some_and(|t| t.contains("Float")));
    let pasted = ok(
        &mut world,
        "editor.matgraph_clipboard",
        json!({ "op": "paste", "pos": [0.0, 200.0] }),
    );
    assert_eq!(pasted["renamed"]["k"], "k_2");
    ok(
        &mut world,
        "editor.matgraph_arrange",
        json!({ "op": "align_left", "nodes": ["k", "k_2"] }),
    );
    let note = ok(
        &mut world,
        "editor.matgraph_annotate",
        json!({ "op": "comment_add", "text": "hi", "min": [0.0, 0.0], "max": [50.0, 50.0] }),
    );
    assert_eq!(note["comments"], 1);
    let err = call(
        &mut world,
        "editor.matgraph_node_preview",
        json!({ "node": "nope" }),
    )
    .expect_err("unknown");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["code"].as_str()),
        Some("matgraph_node_missing")
    );
    let err = call(
        &mut world,
        "editor.matgraph_preview",
        json!({ "shape": "mesh", "mesh": "assets/none.glb" }),
    )
    .expect_err("missing mesh");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["code"].as_str()),
        Some("matgraph_preview_mesh_failed")
    );
    let err =
        call(&mut world, "editor.matgraph_preview", json!({ "shape": "torus" })).expect_err("bad shape");
    assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
    let err = call(&mut world, "editor.matgraph_apply_to_selection", json!({})).expect_err("unsaved");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["code"].as_str()),
        Some("matgraph_apply_refused")
    );
    let _: BTreeMap<String, String> = BTreeMap::new();
}

/// The Stage 2 methods are registered with the editor's agent dispatcher: a request through
/// the inbox opens a graph, search-adds a node, sets a typed row, copies and pastes, arranges,
/// annotates, and the Material panel's `editor.material_preview` answers (or names the missing
/// file).
#[test]
fn stage2_methods_answer_through_the_agent_dispatcher() {
    let dir = regia_test_utils::unique_temp_dir("matgraph-s2-dispatch");
    let mut app = crate::engine::app::App::new();
    app.add_plugins(crate::engine::app::MinimalPlugins);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(dir.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    crate::agent_ext::install(app.world_mut());
    app.finish_plugins();
    let (sender, inbox) = crossbeam_channel::unbounded();
    app.insert_resource(regia_agent::AgentInbox(inbox));
    let mut call = |method: &str, params: Value| {
        let (reply, answer) = crossbeam_channel::bounded(1);
        sender
            .send(regia_agent::server::Inbound {
                request: regia_agent::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("agent inbox");
        app.update();
        answer.try_recv().expect("dispatch answers in one frame")
    };
    let ok = |response: regia_agent::protocol::Response, method: &str| {
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    };
    ok(call("editor.matgraph_open", json!({ "create": true })), "open");
    let added = ok(
        call(
            "editor.matgraph_search",
            json!({ "query": "flo", "add": "Float", "pos": [0.0, 0.0] }),
        ),
        "search",
    );
    let name = added["added"].as_str().expect("name").to_string();
    ok(
        call(
            "editor.matgraph_node_set",
            json!({ "name": name, "field": "value", "value": 0.5 }),
        ),
        "node_set",
    );
    let fields = ok(
        call("editor.matgraph_node_fields", json!({ "name": name })),
        "fields",
    );
    assert_eq!(fields["fields"][0]["value"], 0.5);
    ok(
        call(
            "editor.matgraph_clipboard",
            json!({ "op": "copy", "nodes": [name] }),
        ),
        "copy",
    );
    let pasted = ok(
        call(
            "editor.matgraph_clipboard",
            json!({ "op": "paste", "pos": [0.0, 200.0] }),
        ),
        "paste",
    );
    let copy = pasted["renamed"][name.as_str()]
        .as_str()
        .expect("renamed")
        .to_string();
    ok(
        call(
            "editor.matgraph_arrange",
            json!({ "op": "align_left", "nodes": [name, copy] }),
        ),
        "arrange",
    );
    let note = ok(
        call(
            "editor.matgraph_annotate",
            json!({ "op": "comment_add", "text": "n", "min": [0.0, 0.0], "max": [10.0, 10.0] }),
        ),
        "annotate",
    );
    assert_eq!(note["comments"], 1);
    let missing = call(
        "editor.material_preview",
        json!({ "path": dir.join("none.material.ron").display().to_string() }),
    );
    assert!(missing.error.is_some(), "a missing material file is an error");
    let _ = std::fs::remove_dir_all(&dir);
}

/// w5 (ADR-9970): `editor.matgraph_annotate { op: "comment_set", rect, move_nodes }` through the
/// agent dispatcher moves the frame and the nodes inside it (one undo step), as the canvas
/// header drag does; `reroute_set` answers through it too.
#[test]
fn comment_set_moves_nodes_through_the_agent_dispatcher() {
    let dir = regia_test_utils::unique_temp_dir("matgraph-w5-dispatch");
    let mut app = crate::engine::app::App::new();
    app.add_plugins(crate::engine::app::MinimalPlugins);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(dir.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    crate::agent_ext::install(app.world_mut());
    app.finish_plugins();
    let (sender, inbox) = crossbeam_channel::unbounded();
    app.insert_resource(regia_agent::AgentInbox(inbox));
    let call = |app: &mut crate::engine::app::App, method: &str, params: Value| {
        let (reply, answer) = crossbeam_channel::bounded(1);
        sender
            .send(regia_agent::server::Inbound {
                request: regia_agent::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("agent inbox");
        app.update();
        let response = answer.try_recv().expect("dispatch answers in one frame");
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    };
    call(&mut app, "editor.matgraph_open", json!({ "create": true }));
    let mut names = Vec::new();
    for pos in [[0.0, 0.0], [400.0, 0.0]] {
        let added = call(
            &mut app,
            "editor.matgraph_search",
            json!({ "query": "flo", "add": "Float", "pos": pos }),
        );
        names.push(added["added"].as_str().expect("name").to_string());
    }
    call(
        &mut app,
        "editor.matgraph_annotate",
        json!({ "op": "comment_add", "text": "n", "min": [-10.0, -10.0], "max": [100.0, 100.0] }),
    );
    let depth = app.world().resource::<crate::command::UndoStack>().undo_depth();
    let moved = call(
        &mut app,
        "editor.matgraph_annotate",
        json!({ "op": "comment_set", "index": 0, "rect": [[30.0, -10.0], [140.0, 100.0]], "move_nodes": true }),
    );
    assert_eq!(moved["moved"], json!([names[0]]), "{moved}");
    let editor = app.world().resource::<MatGraphEditor>();
    assert_eq!(editor.doc.layout[&names[0]], [40.0, 0.0], "inside: moved");
    assert_eq!(editor.doc.layout[&names[1]], [400.0, 0.0], "outside: stays");
    assert_eq!(editor.doc.notes.comments[0].min, [30.0, -10.0]);
    assert_eq!(
        app.world().resource::<crate::command::UndoStack>().undo_depth(),
        depth + 1
    );
    let _ = std::fs::remove_dir_all(&dir);
}
