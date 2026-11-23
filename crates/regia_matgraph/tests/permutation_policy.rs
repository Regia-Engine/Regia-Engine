// SPDX-License-Identifier: MPL-2.0
//! The shader-permutation policy on the runtime path (lavapipe in CI; ADR-2026, master plan
//! § 5.5a):
//!
//! - the cook's count of a graph pipeline set (`graph_set_passes`) is the number of pipelines
//!   the renderer creates for it, for a surface graph and a vertex-stage graph;
//! - with a cooked list installed in shipping mode, a graph that is not on it is never built:
//!   its draws use the standard surface (the same frame as the material without its graph), the
//!   frame completes and no pipeline is created for it;
//! - in dev mode an unlisted graph is built with a warning and counted, a listed one is admitted;
//! - a listed graph on a target the list did not count still draws its own pipelines in shipping;
//! - with asynchronous graphs a listed set that is not built yet draws the standard surface and
//!   the frame path compiles nothing.

use std::collections::BTreeMap;

use regia_matgraph::{MaterialGraph, compile, default_params, lab_material};
use regia_render::permutation_policy::{GraphPermutationInput, graph_set_passes};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, GraphTarget, LabMaterial, LabMesh, LabScene,
    LabVertex, PermutationEnforcement, PermutationGuard, PermutationManifest, RenderDevice,
};

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"))
}

const TINT: &str = r#"(
    format_version: 2,
    params: [(name: "tint", value: Color((0.7, 0.25, 0.1)))],
    nodes: {
        "tint": Param("tint"),
        "wp": WorldPosition,
        "x": Swizzle(input: "wp", channels: "x"),
        "k": Float(6.0),
        "xk": Mul("x", "k"),
        "s": Sin("xk"),
        "base": Mul("tint", "s"),
    },
    output: (base_color: Some("base")),
)"#;

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

const LIFTED: &str = r#"(
    format_version: 2,
    params: [(name: "lift", value: Vec3((0.0, 0.3, 0.0)))],
    nodes: {
        "c": Vec3((0.8, 0.35, 0.1)),
        "lift": Param("lift"),
    },
    output: (base_color: Some("c"), world_position_offset: Some("lift")),
    shading: (max_vertex_offset: 2.0),
)"#;

fn graph_material(text: &str) -> LabMaterial {
    let graph = MaterialGraph::from_ron(text).expect("parse graph");
    let compiled = compile(&graph).unwrap_or_else(|e| panic!("{e}"));
    lab_material(&graph, &compiled, default_params(&graph, 0.0), &BTreeMap::new()).expect("graph material")
}

fn input(name: &str, material: &LabMaterial) -> GraphPermutationInput {
    let graph = material.surface_graph.as_deref().expect("a graph material");
    GraphPermutationInput::from_code(name, &graph.code())
}

/// A ground plane (standard material 0) and a cube per extra material.
fn scene(materials: Vec<LabMaterial>) -> LabScene {
    let ground = [
        LabVertex::from_pos_normal_uv([-6.0, 0.0, -6.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([6.0, 0.0, -6.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([6.0, 0.0, 6.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-6.0, 0.0, 6.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    let mut meshes = vec![LabMesh {
        vertices: ground.to_vec(),
        indices: vec![0, 2, 1, 0, 3, 2],
        material: 0,
        ..LabMesh::unskinned()
    }];
    for (i, _) in materials.iter().enumerate() {
        let x = i as f32 * 1.2 - 0.6;
        let quad = [
            LabVertex::from_pos_normal_uv([x - 0.5, 0.1, 0.5], [0.0, 1.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, 0.1, 0.5], [0.0, 1.0, 0.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, 0.1, -0.5], [0.0, 1.0, 0.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x - 0.5, 0.1, -0.5], [0.0, 1.0, 0.0], [0.0, 1.0]),
        ];
        meshes.push(LabMesh {
            vertices: quad.to_vec(),
            indices: vec![0, 1, 2, 0, 2, 3],
            material: i + 1,
            ..LabMesh::unskinned()
        });
    }
    let mut all = vec![LabMaterial {
        base_color: [0.55, 0.5, 0.45, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    }];
    all.extend(materials);
    LabScene {
        width: 96,
        height: 64,
        eye: [0.0, 4.0, 4.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 15_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.4, -0.8, -0.3], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: all,
        meshes,
    }
}

fn quality() -> FrameQuality {
    FrameQuality {
        gpu_driven: false,
        ..FrameQuality::LAB
    }
}

/// The renderer's per-thread pipeline creates while `f` runs.
fn pipelines_during(f: impl FnOnce()) -> u64 {
    let before = regia_render::frame_path_counters();
    f();
    regia_render::frame_path_counters().since(before).pipelines
}

#[test]
fn the_cooked_set_count_is_the_number_of_pipelines_the_renderer_creates() {
    let device = device();
    let mut renderer = FrameRenderer::new();
    // The engine's own pipelines and the graph layouts first (no graph yet).
    renderer
        .prewarm_material_graphs(&device, &[], &[GraphTarget::HDR])
        .expect("engine pipelines");
    let gpu_driven = regia_render::IndirectStrategy::select(&device.capabilities).uses_gpu_objects();
    for (name, text, vertex) in [("tint", TINT, false), ("lifted", LIFTED, true)] {
        let material = graph_material(text);
        let graph = material.surface_graph.as_deref().expect("graph");
        assert_eq!(input(name, &material).vertex, vertex);
        for target in [
            GraphTarget::HDR,
            GraphTarget {
                instanced: false,
                ..GraphTarget::HDR
            },
        ] {
            // The prewarm normalises instancing to the device, as the cook's count assumes a
            // device that has it.
            let instanced = target.instanced && gpu_driven;
            let created = pipelines_during(|| {
                let started = renderer
                    .prewarm_material_graphs(&device, &[graph], &[target])
                    .expect("prewarm");
                assert_eq!(started, 1, "{name} {target:?}");
            });
            let counted = graph_set_passes(instanced, vertex).len() as u64;
            eprintln!("{name} instanced={instanced}: {created} pipelines created, {counted} counted");
            assert_eq!(created, counted, "{name} instanced={instanced}");
        }
    }
}

#[test]
fn an_unlisted_graph_is_never_built_in_shipping_and_draws_the_standard_surface() {
    let device = device();
    let listed = graph_material(STRIPES);
    let unlisted = graph_material(TINT);
    let manifest = PermutationManifest::enumerate("linux", &[input("stripes", &listed)], &BTreeMap::new());
    let guard = PermutationGuard::from_manifest(&manifest, "DesktopLow", PermutationEnforcement::Fallback)
        .expect("guard");

    let lab = scene(vec![unlisted.clone()]);
    let mut renderer = FrameRenderer::new();
    renderer.set_permutation_guard(guard);
    let mut frames = Vec::new();
    for _ in 0..2 {
        let image = renderer
            .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality()))
            .expect("the frame completes");
        frames.push(image.pixels);
    }
    let stats = renderer.permutation_stats();
    let graphs = renderer.material_graph_stats();
    eprintln!("{stats:?} {graphs:?}");
    assert_eq!(stats.refused, 1, "decided once, remembered: {stats:?}");
    assert_eq!(stats.enforcement, "fallback");
    assert_eq!(stats.unlisted_keys.len(), 1);
    assert_eq!(graphs.ready, 0, "{graphs:?}");
    assert_eq!(graphs.built_on_frame_path, 0, "{graphs:?}");
    assert_eq!(
        graphs.fallback_draws, 2,
        "one fallback draw per frame: {graphs:?}"
    );

    // The fallback is the material's standard surface: the frame of the material without its graph.
    let mut standard = unlisted;
    standard.surface_graph = None;
    let reference = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(vec![standard]).as_frame_scene_with_quality(quality()),
        )
        .expect("reference");
    let differing = frames[1]
        .iter()
        .zip(&reference.pixels)
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differing, 0, "the refused graph draws the standard surface");

    // A third frame creates nothing for the refused graph.
    let created = pipelines_during(|| {
        renderer
            .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality()))
            .expect("frame 3");
    });
    assert_eq!(created, 0, "no pipeline for a refused set");
}

#[test]
fn in_dev_an_unlisted_graph_is_built_with_a_warning_and_a_listed_one_is_admitted() {
    let device = device();
    let listed = graph_material(STRIPES);
    let unlisted = graph_material(TINT);
    let manifest = PermutationManifest::enumerate("linux", &[input("stripes", &listed)], &BTreeMap::new());
    let guard = PermutationGuard::from_manifest(&manifest, "DesktopLow", PermutationEnforcement::Warn)
        .expect("guard");
    let lab = scene(vec![listed, unlisted]);
    let mut renderer = FrameRenderer::new();
    renderer.set_permutation_guard(guard);
    renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality()))
        .expect("frame");
    let stats = renderer.permutation_stats();
    let graphs = renderer.material_graph_stats();
    eprintln!("{stats:?} {graphs:?}");
    assert_eq!(
        (stats.admitted, stats.unlisted_built, stats.refused),
        (1, 1, 0),
        "{stats:?}"
    );
    assert_eq!(stats.listed_sets, 1);
    assert_eq!(
        graphs.ready, 2,
        "both graphs drew with their own pipelines: {graphs:?}"
    );
    assert_eq!(graphs.fallback_draws, 0, "{graphs:?}");
}

#[test]
fn a_listed_graph_on_a_target_the_list_did_not_count_keeps_its_own_pipelines_in_shipping() {
    let device = device();
    let listed = graph_material(STRIPES);
    let manifest = PermutationManifest::enumerate("linux", &[input("stripes", &listed)], &BTreeMap::new());
    let guard = PermutationGuard::from_manifest(&manifest, "DesktopLow", PermutationEnforcement::Fallback)
        .expect("guard");
    // The list counted the single-sided set; a scene that turns culling off needs the other one.
    let mut lab = scene(vec![listed]);
    lab.double_sided = true;
    let mut renderer = FrameRenderer::new();
    renderer.set_permutation_guard(guard);
    renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality()))
        .expect("frame");
    let stats = renderer.permutation_stats();
    let graphs = renderer.material_graph_stats();
    eprintln!("{stats:?} {graphs:?}");
    assert_eq!(
        (stats.admitted, stats.off_target, stats.refused),
        (1, 1, 0),
        "{stats:?}"
    );
    assert_eq!(graphs.ready, 1, "{graphs:?}");
    assert_eq!(graphs.fallback_draws, 0, "{graphs:?}");
}

#[test]
fn a_listed_set_still_building_draws_the_standard_surface_and_the_frame_compiles_nothing() {
    let device = device();
    let listed = graph_material(STRIPES);
    let manifest = PermutationManifest::enumerate("linux", &[input("stripes", &listed)], &BTreeMap::new());
    let guard = PermutationGuard::from_manifest(&manifest, "DesktopLow", PermutationEnforcement::Fallback)
        .expect("guard");
    let lab = scene(vec![listed]);
    let mut renderer = FrameRenderer::new();
    renderer.set_async_material_graphs(true);
    renderer.set_permutation_guard(guard);
    let first_frame = renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality()))
        .expect("first frame");
    let first = renderer.material_graph_stats();
    eprintln!("{first:?}");
    // The fallback frame is the material's standard surface, not the graph and not garbage.
    let mut standard = graph_material(STRIPES);
    standard.surface_graph = None;
    let reference = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(vec![standard]).as_frame_scene_with_quality(quality()),
        )
        .expect("reference");
    assert!(
        first_frame.pixels == reference.pixels,
        "the first frame must draw the standard surface"
    );
    assert_eq!(first.built_on_frame_path, 0, "{first:?}");
    assert_eq!(
        first.fallback_draws, 1,
        "the frame did not wait for the set: {first:?}"
    );

    renderer.settle_material_graphs();
    renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality()))
        .expect("second frame");
    let settled = renderer.material_graph_stats();
    eprintln!("{settled:?}");
    assert_eq!(settled.ready, 1, "{settled:?}");
    assert_eq!(settled.built_on_frame_path, 0, "{settled:?}");
    assert_eq!(settled.fallback_draws, 1, "the built set draws: {settled:?}");
    let stats = renderer.permutation_stats();
    assert_eq!((stats.admitted, stats.refused), (1, 0), "{stats:?}");
}

// ---- Bounded growth of the open axes and error mapping (ADR-4901) ----

use regia_matgraph::{
    COMPILE_ERROR_CODES, CompileError, FunctionInput, FunctionSource, MATFUNC_FORMAT_VERSION,
    MAX_LOWERED_NODES, MAX_STATIC_SWITCHES, MaterialFunction, Node, SwitchDecl, ValueType,
    compile_unvalidated, compile_unvalidated_with,
};

/// `f<level>`: `fan` calls of `f<level + 1>` on its input, summed; the last level halves its input.
/// With `distinct`, call `i` takes its input times `i + 1` (every call has its own arguments, so
/// each expands on its own); without, every call takes the input itself (identical calls share
/// one expansion, ADR-7730).
fn nested_function(level: usize, depth: usize, fan: usize, distinct: bool) -> MaterialFunction {
    let mut nodes = BTreeMap::new();
    nodes.insert("x".to_string(), Node::Input("x".into()));
    let out = if level == depth {
        nodes.insert("k".to_string(), Node::Float(0.5));
        nodes.insert("half".to_string(), Node::Mul("x".into(), "k".into()));
        "half".to_string()
    } else {
        let mut sum = String::new();
        for i in 0..fan {
            let call = format!("c{i}");
            let arg = if distinct {
                let (k, m) = (format!("k{i}"), format!("m{i}"));
                nodes.insert(k.clone(), Node::Float(i as f32 + 1.0));
                nodes.insert(m.clone(), Node::Mul("x".into(), k));
                m
            } else {
                "x".to_string()
            };
            nodes.insert(
                call.clone(),
                Node::Call {
                    function: format!("f{}", level + 1),
                    args: BTreeMap::from([("x".to_string(), arg)]),
                    output: None,
                },
            );
            sum = if sum.is_empty() {
                call
            } else {
                let add = format!("s{i}");
                nodes.insert(add.clone(), Node::Add(sum, call));
                add
            };
        }
        sum
    };
    MaterialFunction {
        format_version: MATFUNC_FORMAT_VERSION,
        name: format!("f{level}"),
        inputs: vec![FunctionInput {
            name: "x".into(),
            ty: ValueType::Vec3,
        }],
        nodes,
        outputs: BTreeMap::from([("out".to_string(), out)]),
    }
}

/// A graph whose node `big` calls `f1`, `fan` calls per level, `depth` levels (each call with
/// its own arguments when `distinct`).
fn nested_graph(depth: usize, fan: usize, distinct: bool) -> MaterialGraph {
    let mut graph = MaterialGraph::new("nested");
    for level in 1..=depth {
        graph.functions.insert(
            format!("f{level}"),
            FunctionSource::Inline(nested_function(level, depth, fan, distinct)),
        );
    }
    graph.nodes.insert("c".into(), Node::Vec3([0.8, 0.4, 0.2]));
    graph.nodes.insert(
        "big".into(),
        Node::Call {
            function: "f1".into(),
            args: BTreeMap::from([("x".to_string(), "c".to_string())]),
            output: None,
        },
    );
    graph.output.base_color = Some("big".into());
    graph
}

#[test]
fn function_expansion_past_the_node_bound_fails_on_the_authored_call() {
    // 16 calls per level, 4 levels, each call with its own arguments: 65,536 leaf copies
    // unbounded, far over the bound. (Identical calls share one expansion since ADR-7730, so
    // the calls differ in their arguments to grow.)
    let graph = nested_graph(4, 16, true);
    let errors = match compile_unvalidated(&graph) {
        Ok(compiled) => panic!(
            "an exponential expansion compiled ({} lowered nodes)",
            compiled.lowered_nodes
        ),
        Err(errors) => errors.0,
    };
    let bound: Vec<&CompileError> = errors
        .iter()
        .filter(|e| e.message.contains(&MAX_LOWERED_NODES.to_string()))
        .collect();
    assert_eq!(bound.len(), 1, "one bound error: {errors:?}");
    assert_eq!(bound[0].code(), "node");
    assert_eq!(bound[0].authored_node(), Some("big"), "{:?}", bound[0]);
}

#[test]
fn a_bounded_expansion_compiles_and_reports_its_calls_and_nodes() {
    // 3 calls per level, 3 levels: 1 + 3 + 9 calls.
    let graph = nested_graph(3, 3, true);
    let compiled = compile_unvalidated(&graph).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(compiled.function_calls, 13);
    // ADR-7730: the same nesting with identical calls expands once per level (1 + 1 + 1), and
    // even 16 identical calls on 4 levels stay far under the bound.
    let shared = compile_unvalidated(&nested_graph(3, 3, false)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(shared.function_calls, 3);
    let wide = compile_unvalidated(&nested_graph(4, 16, false)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(wide.function_calls, 4);
    assert!(
        wide.lowered_nodes < MAX_LOWERED_NODES / 10,
        "{}",
        wide.lowered_nodes
    );
    assert!(
        compiled.lowered_nodes > graph.nodes.len() && compiled.lowered_nodes <= MAX_LOWERED_NODES,
        "{} lowered nodes",
        compiled.lowered_nodes
    );
    // The plain graph lowers to itself: no calls, its own node count.
    let plain = MaterialGraph::from_ron(TINT).expect("parse");
    let compiled = compile_unvalidated(&plain).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(compiled.function_calls, 0);
    assert_eq!(compiled.lowered_nodes, plain.nodes.len());
}

fn switched_graph(switches: usize) -> MaterialGraph {
    let mut graph = MaterialGraph::from_ron(TINT).expect("parse");
    for i in 0..switches {
        graph.switches.push(SwitchDecl {
            name: format!("s{i}"),
            default: i % 2 == 0,
        });
    }
    graph
}

#[test]
fn static_switches_are_bounded_and_the_one_past_the_bound_is_named() {
    let at_bound = switched_graph(MAX_STATIC_SWITCHES);
    let compiled = compile_unvalidated(&at_bound).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(compiled.switches.len(), MAX_STATIC_SWITCHES);
    let over = switched_graph(MAX_STATIC_SWITCHES + 1);
    let errors = compile_unvalidated(&over).expect_err("one switch too many").0;
    let named = format!("switch:s{MAX_STATIC_SWITCHES}");
    let error = errors
        .iter()
        .find(|e| e.node.as_deref() == Some(named.as_str()))
        .unwrap_or_else(|| panic!("no error on {named}: {errors:?}"));
    assert_eq!(error.code(), "switch");
    assert_eq!(error.authored_node(), None);
}

#[test]
fn switch_permutations_have_distinct_code_and_errors_name_authored_nodes() {
    // Each value of a switch that picks a different branch is its own pipeline key, so the cook
    // counts every picked permutation; the unpicked branch is never emitted.
    let text = r#"(
        format_version: 2,
        switches: [(name: "warm", default: true)],
        nodes: {
            "a": Vec3((0.9, 0.4, 0.1)),
            "b": Vec3((0.1, 0.4, 0.9)),
            "pick": Switch(switch: "warm", on: "a", off: "b"),
        },
        output: (base_color: Some("pick")),
    )"#;
    let graph = MaterialGraph::from_ron(text).expect("parse");
    let on = compile_unvalidated(&graph).unwrap_or_else(|e| panic!("{e}"));
    let off = compile_unvalidated_with(&graph, &BTreeMap::from([("warm".to_string(), false)]))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(on.key, off.key, "two switch values, two pipeline keys");
    // A bad node inside an expanded function points at the authored call.
    let mut graph = nested_graph(1, 1, false);
    if let Some(FunctionSource::Inline(f)) = graph.functions.get_mut("f1") {
        f.nodes.insert("bad".into(), Node::Saturate("missing".into()));
        f.outputs.insert("out".into(), "bad".into());
    }
    let errors = compile_unvalidated(&graph).expect_err("a missing input").0;
    let inner = errors
        .iter()
        .find(|e| e.node.as_deref().is_some_and(|n| n.starts_with("big/")))
        .unwrap_or_else(|| panic!("no error inside the call: {errors:?}"));
    assert_eq!(inner.code(), "node");
    assert_eq!(inner.authored_node(), Some("big"));
    // Every code is stable lowercase ascii; the unmapped one has no authored node.
    for code in COMPILE_ERROR_CODES {
        assert!(
            code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{code}"
        );
    }
    let unmapped = CompileError {
        node: None,
        message: format!("{}: x", regia_matgraph::GENERATED_CODE_INVALID_PREFIX),
    };
    assert_eq!(unmapped.code(), "generated_code_invalid");
    assert_eq!(unmapped.authored_node(), None);
    assert!(COMPILE_ERROR_CODES.contains(&unmapped.code()));
}

/// Every profile of every shipped platform: the cook's enumeration is bounded (under budget) and
/// the runtime's warm of the same graphs for the profile's target builds exactly the listed
/// sets and the counted pipelines, no more (ADR-4901). The instanced passes a device without
/// GPU-driven draws drops are the only difference, and the target normalises them.
#[test]
fn every_shipped_profile_warms_exactly_the_sets_its_cook_lists() {
    use regia_render::permutation_policy::{ProfileTarget, SHIPPED_PLATFORMS};
    use regia_render::{RenderProfile, RenderProfilePreset};
    let device = device();
    let gpu_driven = regia_render::IndirectStrategy::select(&device.capabilities).uses_gpu_objects();
    let materials = [
        graph_material(TINT),
        graph_material(STRIPES),
        graph_material(LIFTED),
    ];
    let inputs: Vec<GraphPermutationInput> = ["tint", "stripes", "lifted"]
        .iter()
        .zip(&materials)
        .map(|(name, m)| input(name, m))
        .collect();
    let graphs: Vec<&regia_render::SurfaceGraph> = materials
        .iter()
        .map(|m| m.surface_graph.as_deref().expect("graph"))
        .collect();
    let mut profiles = 0;
    for platform in SHIPPED_PLATFORMS {
        let manifest = PermutationManifest::enumerate(platform, &inputs, &BTreeMap::new());
        assert!(
            manifest.budget_problems().is_empty(),
            "{:?}",
            manifest.budget_problems()
        );
        for listed in &manifest.profiles {
            let preset = RenderProfilePreset::from_name(&listed.profile).expect("a preset name");
            let target = ProfileTarget::of(&RenderProfile::from_preset(preset));
            let graph_target = target.graph_target(&device.capabilities);
            assert_eq!(listed.entries.len(), graphs.len(), "{}", listed.summary_line());
            let expected: u64 = listed
                .entries
                .iter()
                .map(|e| {
                    graph_set_passes(graph_target.instanced, e.passes.iter().any(|p| p == "velocity")).len()
                        as u64
                })
                .sum();
            if graph_target.instanced == target.instanced {
                assert_eq!(
                    expected,
                    u64::from(listed.pipelines),
                    "{platform}/{}",
                    listed.profile
                );
            }
            let mut renderer = FrameRenderer::new();
            renderer
                .prewarm_material_graphs(&device, &[], &[GraphTarget::HDR])
                .expect("engine pipelines");
            let mut started = 0;
            let created = pipelines_during(|| {
                started = renderer
                    .prewarm_material_graphs(&device, &graphs, &[graph_target])
                    .expect("prewarm");
            });
            eprintln!(
                "{platform}/{}: {} sets listed, {started} warmed, {created} pipelines created ({} counted, gpu_driven {gpu_driven})",
                listed.profile,
                listed.entries.len(),
                expected
            );
            assert_eq!(started, listed.entries.len(), "{platform}/{}", listed.profile);
            assert_eq!(created, expected, "{platform}/{}", listed.profile);
            // A second warm of the same list builds nothing: the set is bounded, not per frame.
            let again = pipelines_during(|| {
                renderer
                    .prewarm_material_graphs(&device, &graphs, &[graph_target])
                    .expect("prewarm again");
            });
            assert_eq!(again, 0, "{platform}/{}", listed.profile);
            profiles += 1;
        }
    }
    assert_eq!(
        profiles,
        SHIPPED_PLATFORMS
            .iter()
            .map(|p| regia_render::permutation_policy::platform_presets(p).len())
            .sum::<usize>()
    );
}
