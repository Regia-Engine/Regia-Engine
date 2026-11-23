// SPDX-License-Identifier: MPL-2.0
//! Public behavior of the format, instance, cooked, material, cache and library layers that the
//! in-crate unit tests and the GPU parity tests do not reach: error variants, defaults, hash
//! sensitivity, cooked-payload checks and the shading-to-material mapping. No GPU.

use std::collections::BTreeMap;

use regia_asset::AssetId;
use regia_core::version::VersionStep;
use regia_matgraph::{
    COOKED_MATGRAPH_VERSION, ColorSpace, CookedGraphError, CookedGraphTexture, CookedMaterialGraph,
    FormatError, FunctionSource, GraphAlphaMode, InstanceError, MATFUNC_FORMAT_VERSION,
    MATGRAPH_FORMAT_VERSION, MaterialBuildError, MaterialFunction, MaterialGraph, MaterialGraphLibrary,
    MaterialInstance, Node, NormalSpace, ParamDecl, ParamValue, ShaderCache, SwitchDecl, TextureDecl,
    compile, compile_unvalidated, default_params, graph_hash, instance_material, lab_material,
    permutation_hash, thread_compile_count,
};
use regia_render::surface_graph::{GRAPH_PARAM_SLOTS, GRAPH_TEXTURE_SLOTS};
use regia_render::{ALPHA_ADDITIVE, ALPHA_BLEND, ALPHA_MASK, ALPHA_OPAQUE, ALPHA_PREMULTIPLIED, LabTexture};

fn s(text: &str) -> String {
    text.to_string()
}

fn pixel() -> LabTexture {
    LabTexture {
        width: 1,
        height: 1,
        rgba: vec![255; 4],
    }
}

/// `base = tint * albedo.rgb`, one parameter, one texture.
fn textured() -> MaterialGraph {
    let mut g = MaterialGraph::new("textured");
    g.params
        .push(ParamDecl::new(s("tint"), ParamValue::Color([0.5, 0.5, 0.5])));
    g.textures.push(TextureDecl {
        name: s("albedo"),
        path: s("assets/a.png"),
        color_space: ColorSpace::Srgb,
    });
    g.nodes.insert(s("uv"), Node::TexCoord);
    g.nodes.insert(
        s("s"),
        Node::Sample {
            texture: s("albedo"),
            uv: s("uv"),
            lod: None,
        },
    );
    g.nodes.insert(
        s("rgb"),
        Node::Swizzle {
            input: s("s"),
            channels: s("rgb"),
        },
    );
    g.nodes.insert(s("tint"), Node::Param(s("tint")));
    g.nodes.insert(s("base"), Node::Mul(s("rgb"), s("tint")));
    g.output.base_color = Some(s("base"));
    g
}

/// A graph with a vertex offset (`world_position_offset`).
fn waving() -> MaterialGraph {
    let mut g = MaterialGraph::new("waving");
    g.nodes.insert(s("p"), Node::WorldPosition);
    g.nodes.insert(s("t"), Node::Time);
    g.nodes.insert(s("w"), Node::Sin(s("t")));
    g.nodes.insert(s("off"), Node::Mul(s("p"), s("w")));
    g.output.world_position_offset = Some(s("off"));
    g
}

// -- Format ----------------------------------------------------------------------------------

#[test]
fn malformed_documents_are_ron_errors_and_newer_ones_are_named() {
    for text in ["", "(", "not ron at all", "(format_version: 2, nodes: 7)"] {
        assert!(
            matches!(MaterialGraph::from_ron(text), Err(FormatError::Ron(_))),
            "{text:?}"
        );
    }
    let newer = format!("(format_version: {})", MATGRAPH_FORMAT_VERSION + 1);
    match MaterialGraph::from_ron(&newer) {
        Err(FormatError::NewerVersion { found, expected }) => {
            assert_eq!(found, MATGRAPH_FORMAT_VERSION + 1);
            assert_eq!(expected, MATGRAPH_FORMAT_VERSION);
        }
        other => panic!("expected NewerVersion, got {other:?}"),
    }
    let message = MaterialGraph::from_ron(&newer).expect_err("newer").to_string();
    assert!(message.contains("upgrade the engine"), "{message}");
}

#[test]
fn omitted_optional_fields_take_their_documented_defaults() {
    let graph = MaterialGraph::from_ron(
        r#"(
            format_version: 2,
            textures: [(name: "t", path: "t.png", color_space: Linear)],
            nodes: {
                "uv": TexCoord,
                "f": Float(1.0),
                "s": Sample(texture: "t", uv: "uv"),
                "tri": Triplanar(texture: "t", scale: "f"),
                "xf": UvTransform(uv: "uv"),
                "hb": HeightBlend(a: "f", b: "f", height_a: "f", height_b: "f", t: "f"),
            },
            // `output` itself is required (docs/formats/matgraph-ron.md); every field in it is
            // optional.
            output: (),
        )"#,
    )
    .expect("parse");
    assert_eq!(graph.name, "");
    assert!(graph.params.is_empty() && graph.switches.is_empty() && graph.functions.is_empty());
    assert_eq!(graph.shading.alpha_mode, GraphAlphaMode::Opaque);
    assert!(graph.shading.specular_occlusion && graph.shading.multi_scatter);
    assert!(!graph.shading.unlit);
    assert_eq!(graph.shading.max_vertex_offset, 0.0);
    assert_eq!(graph.output.normal_space, NormalSpace::default());
    assert!(matches!(graph.nodes["s"], Node::Sample { lod: None, .. }));
    assert!(matches!(
        graph.nodes["tri"],
        Node::Triplanar { sharpness, .. } if sharpness == 4.0
    ));
    assert!(matches!(
        graph.nodes["xf"],
        Node::UvTransform { scale, offset, rotation, .. }
            if scale == [1.0, 1.0] && offset == [0.0, 0.0] && rotation == 0.0
    ));
    assert!(matches!(
        graph.nodes["hb"],
        Node::HeightBlend { depth, .. } if (depth - 0.2).abs() < 1e-6
    ));
}

#[test]
fn a_graph_with_switches_functions_and_shading_round_trips_through_ron() {
    let mut g = textured();
    g.switches.push(SwitchDecl {
        name: s("detail"),
        default: false,
    });
    g.functions.insert(
        s("pass"),
        FunctionSource::Inline(MaterialFunction {
            format_version: MATFUNC_FORMAT_VERSION,
            name: s("pass"),
            inputs: Vec::new(),
            nodes: BTreeMap::from([(s("one"), Node::Float(1.0))]),
            outputs: BTreeMap::from([(s("out"), s("one"))]),
        }),
    );
    g.functions
        .insert(s("external"), FunctionSource::File(s("fn/external.matfunc.ron")));
    g.shading.alpha_mode = GraphAlphaMode::Mask(0.25);
    g.shading.clearcoat = 0.5;
    g.shading.max_vertex_offset = 0.75;
    g.output.normal_space = NormalSpace::World;
    let text = g.to_ron().expect("write");
    assert_eq!(MaterialGraph::from_ron(&text).expect("read"), g);
}

#[test]
fn migration_errors_name_the_failing_step() {
    fn failing(_: &mut MaterialGraph) -> Result<(), String> {
        Err(s("boom"))
    }
    let steps = [VersionStep {
        from: 1,
        to: 2,
        migrate: failing,
    }];
    let mut g = MaterialGraph::new("old");
    g.format_version = 1;
    match g.migrate_with(&steps) {
        Err(FormatError::Migration(message)) => assert!(message.contains("boom"), "{message}"),
        other => panic!("expected Migration, got {other:?}"),
    }
    // A gap in the chain is an error too, and a current graph needs no step.
    let mut gap = MaterialGraph::new("gap");
    gap.format_version = 0;
    assert!(matches!(gap.migrate_with(&steps), Err(FormatError::Migration(_))));
    let mut current = MaterialGraph::new("current");
    assert_eq!(
        current.migrate_with(&[]).expect("current"),
        MATGRAPH_FORMAT_VERSION
    );
    let mut future = MaterialGraph::new("future");
    future.format_version = MATGRAPH_FORMAT_VERSION + 1;
    assert!(matches!(
        future.migrate_with(&[]),
        Err(FormatError::NewerVersion { .. })
    ));
}

#[test]
fn material_function_documents_validate_version_and_fields() {
    let ok = r#"(
        format_version: 1,
        name: "one",
        nodes: {"k": Float(1.0)},
        outputs: {"out": "k"},
    )"#;
    let function = MaterialFunction::from_ron(ok).expect("parse");
    assert_eq!(function.name, "one");
    assert!(function.inputs.is_empty());
    assert_eq!(
        MaterialFunction::from_ron(&function.to_ron().expect("write")).expect("back"),
        function
    );
    let newer = ok.replace(
        "format_version: 1",
        &format!("format_version: {}", MATFUNC_FORMAT_VERSION + 1),
    );
    assert!(matches!(
        MaterialFunction::from_ron(&newer),
        Err(FormatError::NewerVersion { .. })
    ));
    let unknown = ok.replace("name: \"one\",", "name: \"one\", surprise: 1,");
    assert!(matches!(
        MaterialFunction::from_ron(&unknown),
        Err(FormatError::Ron(_))
    ));
}

#[test]
fn resolving_function_files_inlines_them_and_reports_bad_ones() {
    let file = r#"(format_version: 1, nodes: {"k": Float(1.0)}, outputs: {"out": "k"})"#;
    let mut g = MaterialGraph::new("g");
    g.functions
        .insert(s("b"), FunctionSource::File(s("fn/b.matfunc.ron")));
    g.functions
        .insert(s("a"), FunctionSource::File(s("fn/a.matfunc.ron")));
    g.functions.insert(
        s("inline"),
        FunctionSource::Inline(MaterialFunction {
            format_version: MATFUNC_FORMAT_VERSION,
            name: s("inline"),
            inputs: Vec::new(),
            nodes: BTreeMap::from([(s("k"), Node::Float(2.0))]),
            outputs: BTreeMap::from([(s("out"), s("k"))]),
        }),
    );
    let mut asked = Vec::new();
    let paths = g
        .resolve_function_files(&mut |path| {
            asked.push(path.to_string());
            Ok(file.to_string())
        })
        .expect("resolve");
    // Name order, and inline functions are never read.
    assert_eq!(paths, vec![s("fn/a.matfunc.ron"), s("fn/b.matfunc.ron")]);
    assert_eq!(asked, paths);
    assert!(
        g.functions
            .values()
            .all(|f| matches!(f, FunctionSource::Inline(_)))
    );
    // Nothing left to read the second time.
    let again = g
        .resolve_function_files(&mut |_| Err(s("must not be called")))
        .expect("nothing to resolve");
    assert!(again.is_empty());

    for (text, wanted) in [
        ("(", "`bad`"),
        (r#"(format_version: 9, nodes: {}, outputs: {})"#, "newer"),
    ] {
        let mut g = MaterialGraph::new("g");
        g.functions
            .insert(s("bad"), FunctionSource::File(s("fn/bad.matfunc.ron")));
        let error = g
            .resolve_function_files(&mut |_| Ok(text.to_string()))
            .expect_err("bad function file");
        assert!(matches!(error, FormatError::Function(_)), "{error:?}");
        let message = error.to_string();
        assert!(message.contains(wanted), "{message}");
        assert!(message.contains("fn/bad.matfunc.ron"), "{message}");
    }
}

// -- Hash and cache --------------------------------------------------------------------------

#[test]
fn the_hash_ignores_values_paths_names_and_shading_but_not_layout() {
    let base = textured();
    let hash = graph_hash(&base);
    assert_eq!(hash, graph_hash(&base.clone()), "deterministic");

    let mut same = base.clone();
    same.name = s("renamed");
    same.params[0].value = ParamValue::Color([1.0, 0.0, 0.0]);
    same.textures[0].path = s("assets/other.png");
    same.shading.clearcoat = 1.0;
    same.shading.alpha_mode = GraphAlphaMode::Blend;
    assert_eq!(hash, graph_hash(&same));

    let mut retyped = base.clone();
    retyped.params[0].value = ParamValue::Vec3([0.5; 3]);
    assert_eq!(hash, graph_hash(&retyped), "Color and Vec3 are the same type");
    retyped.params[0].value = ParamValue::Vec4([0.5; 4]);
    assert_ne!(hash, graph_hash(&retyped), "a wider parameter changes the layout");

    let mut space = base.clone();
    space.textures[0].color_space = ColorSpace::Linear;
    assert_ne!(hash, graph_hash(&space));

    let mut normal = base.clone();
    normal.output.normal_space = NormalSpace::World;
    assert_ne!(hash, graph_hash(&normal));

    let mut renamed_param = base.clone();
    renamed_param.params[0].name = s("tint2");
    assert_ne!(hash, graph_hash(&renamed_param));
}

#[test]
fn switch_overrides_that_match_the_default_hash_like_no_override() {
    let mut g = textured();
    g.switches.push(SwitchDecl {
        name: s("a"),
        default: true,
    });
    let none = permutation_hash(&g, &BTreeMap::new());
    let same = permutation_hash(&g, &BTreeMap::from([(s("a"), true)]));
    let flipped = permutation_hash(&g, &BTreeMap::from([(s("a"), false)]));
    let undeclared = permutation_hash(&g, &BTreeMap::from([(s("ghost"), true)]));
    assert_eq!(none, same);
    assert_ne!(none, flipped);
    assert_eq!(none, undeclared, "undeclared overrides are not read by the hash");
}

#[test]
fn compiling_counts_on_the_calling_thread() {
    let g = textured();
    // The calling thread's count: the process-wide `compile_count` also moves with the tests
    // compiling in parallel.
    let before = thread_compile_count();
    compile_unvalidated(&g).expect("compile");
    assert_eq!(thread_compile_count(), before + 1);
    let mut broken = g.clone();
    broken.output.alpha = Some(s("ghost"));
    compile_unvalidated(&broken).expect_err("broken");
    assert_eq!(thread_compile_count(), before + 2, "failed compiles count too");
}

#[test]
fn the_cache_shares_structure_not_values_and_never_stores_failures() {
    let mut cache = ShaderCache::new();
    let a = textured();
    let (first, hit) = cache.get_or_compile(&a).expect("first");
    assert!(!hit);
    let mut b = a.clone();
    b.params[0].value = ParamValue::Color([0.0, 1.0, 0.0]);
    b.textures[0].path = s("elsewhere.png");
    let (second, hit) = cache.get_or_compile(&b).expect("second");
    assert!(hit, "a value-only change shares the compiled graph");
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    assert_eq!(cache.stats().hits, 1);
    assert_eq!(cache.stats().misses, 1);

    let mut broken = a.clone();
    broken.output.alpha = Some(s("ghost"));
    let before = cache.stats();
    assert!(cache.get_or_compile(&broken).is_err());
    assert!(cache.get_or_compile(&broken).is_err());
    assert_eq!(
        cache.stats(),
        before,
        "failures change no counter and add no entry"
    );

    let mut structural = a.clone();
    structural.output.roughness = Some(s("ghost_free"));
    structural.nodes.insert(s("ghost_free"), Node::Float(0.5));
    let (third, hit) = cache.get_or_compile(&structural).expect("third");
    assert!(!hit);
    assert_ne!(third.key, first.key);
    assert_eq!(cache.stats().entries, 2);

    cache.clear();
    let cleared = cache.stats();
    assert_eq!(cleared.entries, 0);
    assert_eq!((cleared.hits, cleared.misses), (1, 2), "clear keeps the counters");
    let (_, hit) = cache.get_or_compile(&a).expect("after clear");
    assert!(!hit);
}

#[test]
fn the_library_edits_compile_and_remove_graphs_by_name() {
    let mut library = MaterialGraphLibrary::new();
    library.set("zeta", textured());
    library.set("alpha", waving());
    assert_eq!(library.names().collect::<Vec<_>>(), vec!["alpha", "zeta"]);
    assert_eq!(
        library.get("zeta").map(|e| e.graph.name.as_str()),
        Some("textured")
    );
    assert!(library.get("ghost").is_none());

    let (compiled, hit) = library.compile("zeta").expect("known").expect("compiles");
    assert!(!hit);
    let (again, hit) = library.compile("zeta").expect("known").expect("compiles");
    assert!(hit);
    assert!(std::sync::Arc::ptr_eq(&compiled, &again));
    assert_eq!(library.cache_stats().entries, 1);

    // The vertex graph compiles through the library too, and an invalid graph is an error
    // value, not a missing entry.
    let (wave, _) = library.compile("alpha").expect("known").expect("compiles");
    assert!(wave.vertex_wgsl.is_some());
    let mut broken = textured();
    broken.output.alpha = Some(s("ghost"));
    let revision = library.set("zeta", broken);
    assert_eq!(revision, 3);
    assert!(library.compile("zeta").expect("known").is_err());

    // A switch permutation of a graph without that switch is an error, not a cache hit.
    let overrides = BTreeMap::from([(s("nope"), true)]);
    assert!(library.compile_with("alpha", &overrides).expect("known").is_err());
    assert!(library.remove("zeta"));
    assert!(library.compile("zeta").is_none());
    assert!(library.compile_with("zeta", &BTreeMap::new()).is_none());
}

// -- Instances -------------------------------------------------------------------------------

#[test]
fn instance_documents_reject_unknown_fields_and_bad_ron() {
    for text in [
        "",
        "(",
        "(format_version: 2)",
        r#"(format_version: 2, parent: "p", extra: 1)"#,
    ] {
        assert!(
            matches!(MaterialInstance::from_ron(text), Err(FormatError::Ron(_))),
            "{text:?}"
        );
    }
    let instance = MaterialInstance::from_ron(
        r#"(
            format_version: 2,
            parent: "p",
            params: {"tint": Color((1.0, 0.0, 0.0))},
            textures: {"albedo": "b.png"},
            switches: {"detail": true},
        )"#,
    )
    .expect("parse");
    assert_eq!(instance.params["tint"], ParamValue::Color([1.0, 0.0, 0.0]));
    assert_eq!(instance.textures["albedo"], "b.png");
    assert!(instance.switches["detail"]);
    assert_eq!(
        MaterialInstance::from_ron(&instance.to_ron().expect("write")).expect("back"),
        instance
    );
}

#[test]
fn the_parameter_block_pads_unset_slots_and_validates_before_building() {
    let g = textured();
    let block = MaterialInstance::new("p").param_block(&g, 2.5).expect("block");
    assert_eq!(block.len(), GRAPH_PARAM_SLOTS);
    assert_eq!(block[0], [2.5, 0.0, 0.0, 0.0]);
    assert_eq!(block[1], [0.5, 0.5, 0.5, 0.0]);
    assert!(block[2..].iter().all(|slot| *slot == [0.0; 4]));
    assert_eq!(block, default_params(&g, 2.5));

    let mut wide = MaterialInstance::new("p");
    wide.set("tint", ParamValue::Vec4([1.0; 4]));
    assert_eq!(
        wide.param_block(&g, 0.0),
        Err(InstanceError::TypeMismatch {
            name: s("tint"),
            expected: regia_matgraph::ValueType::Vec3,
            found: regia_matgraph::ValueType::Vec4,
        })
    );
    let mut infinite = MaterialInstance::new("p");
    infinite.set("tint", ParamValue::Vec3([0.0, f32::INFINITY, 0.0]));
    assert_eq!(
        infinite.param_block(&g, 0.0),
        Err(InstanceError::NotFinite(s("tint")))
    );
    let message = InstanceError::UnknownSwitch(s("x")).to_string();
    assert!(message.contains("`x`") && message.contains("switch"), "{message}");
}

// -- Cooked graphs ---------------------------------------------------------------------------

#[test]
fn cooking_asks_for_images_only_for_non_empty_paths() {
    let mut g = textured();
    g.textures.push(TextureDecl {
        name: s("unset"),
        path: String::new(),
        color_space: ColorSpace::Linear,
    });
    let mut asked = Vec::new();
    let id = AssetId::new_random();
    let cooked = CookedMaterialGraph::cook(&g, None, &mut |path| {
        asked.push(path.to_string());
        Some(id)
    })
    .expect("cook");
    assert_eq!(asked, vec![s("assets/a.png")]);
    assert_eq!(cooked.textures.len(), 2);
    assert_eq!(cooked.textures[0].image, Some(id));
    assert!(cooked.textures[0].srgb);
    assert_eq!(cooked.textures[1].image, None, "an empty path binds white");
    assert!(!cooked.textures[1].srgb);
    assert_eq!(cooked.params.len(), GRAPH_PARAM_SLOTS);
    assert_eq!(cooked.version, COOKED_MATGRAPH_VERSION);
    assert_eq!(cooked.name, "textured");
    assert_eq!(cooked.code().key(), compile(&g).expect("compile").key);

    // A missing cooked image also binds white.
    let missing = CookedMaterialGraph::cook(&g, None, &mut |_| None).expect("cook");
    assert!(missing.textures.iter().all(|t| t.image.is_none()));
}

#[test]
fn an_instance_cooks_under_its_parent_name_with_its_own_paths() {
    let g = textured();
    let mut instance = MaterialInstance::new("assets/textured.matgraph.ron");
    instance.textures.insert(s("albedo"), s("assets/override.png"));
    instance.set("tint", ParamValue::Color([1.0, 0.0, 0.0]));
    let cooked = CookedMaterialGraph::cook(&g, Some(&instance), &mut |_| None).expect("cook");
    assert_eq!(cooked.name, "assets/textured.matgraph.ron");
    assert_eq!(cooked.textures[0].path, "assets/override.png");
    assert_eq!(cooked.params[1], [1.0, 0.0, 0.0, 0.0]);
}

#[test]
fn cook_failures_carry_the_compile_or_instance_error() {
    let mut broken = textured();
    broken.output.alpha = Some(s("ghost"));
    let error = CookedMaterialGraph::cook(&broken, None, &mut |_| None).expect_err("compile");
    assert!(matches!(error, CookedGraphError::Compile(_)), "{error:?}");
    assert!(error.to_string().contains("ghost"), "{error}");

    let mut instance = MaterialInstance::new("p");
    instance.set("gloss", ParamValue::Float(1.0));
    let error = CookedMaterialGraph::cook(&textured(), Some(&instance), &mut |_| None).expect_err("instance");
    assert!(
        matches!(error, CookedGraphError::Instance(InstanceError::UnknownParam(ref name)) if name == "gloss"),
        "{error:?}"
    );
}

#[test]
fn a_cooked_payload_with_a_bad_texture_slot_or_garbage_is_rejected() {
    let mut cooked = CookedMaterialGraph::cook(&textured(), None, &mut |_| None).expect("cook");
    let bad_slot = u8::try_from(GRAPH_TEXTURE_SLOTS).expect("small");
    cooked.textures.push(CookedGraphTexture {
        slot: bad_slot,
        srgb: false,
        image: None,
        path: String::new(),
    });
    let bytes = cooked.to_postcard().expect("encode");
    assert!(matches!(
        CookedMaterialGraph::from_postcard(&bytes),
        Err(CookedGraphError::TextureSlot(slot)) if slot == bad_slot
    ));
    assert!(CookedMaterialGraph::from_postcard(&[]).is_err());
    assert!(CookedMaterialGraph::from_postcard(&[0xff; 16]).is_err());

    // Every valid slot index is accepted.
    cooked.textures.pop();
    cooked.textures.push(CookedGraphTexture {
        slot: bad_slot - 1,
        srgb: true,
        image: None,
        path: s("last"),
    });
    let bytes = cooked.to_postcard().expect("encode");
    assert_eq!(
        CookedMaterialGraph::from_postcard(&bytes).expect("decode"),
        cooked
    );
}

#[test]
fn cooked_materials_follow_the_shading_block_and_skip_missing_images() {
    let mut g = waving();
    g.shading.max_vertex_offset = 0.75;
    g.shading.unlit = true;
    g.shading.alpha_mode = GraphAlphaMode::Additive;
    let cooked = CookedMaterialGraph::cook(&g, None, &mut |_| None).expect("cook");
    assert!(cooked.uses_time);
    assert!(cooked.vertex_wgsl.is_some());
    let material = cooked.to_material(&BTreeMap::new());
    let surface = material.surface_graph.as_ref().expect("surface");
    assert!(surface.uses_time);
    assert_eq!(surface.max_vertex_offset, 0.75);
    assert!(material.unlit);
    assert_eq!(material.alpha_mode, ALPHA_ADDITIVE);

    // A negative bound is clamped, and a graph without a vertex stage ignores the bound.
    let mut negative = cooked.clone();
    negative.shading.max_vertex_offset = -3.0;
    let surface = negative
        .to_material(&BTreeMap::new())
        .surface_graph
        .expect("surface");
    assert_eq!(surface.max_vertex_offset, 0.0);
    let mut flat = cooked;
    flat.vertex_wgsl = None;
    let surface = flat.to_material(&BTreeMap::new()).surface_graph.expect("surface");
    assert_eq!(surface.max_vertex_offset, 0.0);

    // An image supplied for a slot the cook did not bind is ignored; a missing one binds none.
    let bound = CookedMaterialGraph::cook(&textured(), None, &mut |_| None).expect("cook");
    let mut images = BTreeMap::new();
    images.insert(5u8, pixel());
    let surface = bound.to_material(&images).surface_graph.expect("surface");
    assert!(surface.textures.iter().all(Option::is_none));
    images.insert(0u8, pixel());
    let surface = bound.to_material(&images).surface_graph.expect("surface");
    assert!(surface.textures[0].is_some());
    assert!(surface.textures[1..].iter().all(Option::is_none));
}

// -- Materials -------------------------------------------------------------------------------

#[test]
fn every_alpha_mode_maps_to_a_material_mode_and_cutoff() {
    let cases = [
        (GraphAlphaMode::Opaque, ALPHA_OPAQUE, 0.5),
        (GraphAlphaMode::Mask(0.125), ALPHA_MASK, 0.125),
        (GraphAlphaMode::Blend, ALPHA_BLEND, 0.5),
        (GraphAlphaMode::Premultiplied, ALPHA_PREMULTIPLIED, 0.5),
        (GraphAlphaMode::Additive, ALPHA_ADDITIVE, 0.5),
    ];
    let mut images = BTreeMap::new();
    images.insert(s("albedo"), pixel());
    for (mode, expected, cutoff) in cases {
        let mut g = textured();
        g.shading.alpha_mode = mode;
        let compiled = compile_unvalidated(&g).expect("compile");
        let material = lab_material(&g, &compiled, default_params(&g, 0.0), &images).expect("material");
        assert_eq!(material.alpha_mode, expected, "{mode:?}");
        assert!((material.alpha_cutoff - cutoff).abs() < 1e-6, "{mode:?}");
    }
}

#[test]
fn shading_flags_and_lobes_reach_the_hard_surface_block() {
    let mut g = textured();
    g.shading.unlit = true;
    g.shading.clearcoat = 0.4;
    g.shading.clearcoat_roughness = 0.2;
    g.shading.anisotropy = -0.5;
    g.shading.anisotropy_rotation = 1.25;
    g.shading.specular_occlusion = false;
    g.shading.multi_scatter = false;
    let compiled = compile_unvalidated(&g).expect("compile");
    let images = BTreeMap::from([(s("albedo"), pixel())]);
    let material = lab_material(&g, &compiled, default_params(&g, 0.0), &images).expect("material");
    assert!(material.unlit);
    let hs = &material.hard_surface;
    assert_eq!(hs.clearcoat, 0.4);
    assert_eq!(hs.clearcoat_roughness, 0.2);
    assert_eq!(hs.anisotropy, -0.5);
    assert_eq!(hs.anisotropy_rotation, 1.25);
    assert_eq!(hs.specular_occlusion, 0.0);
    assert_eq!(hs.multi_scatter, 0.0);
}

#[test]
fn a_vertex_graph_carries_its_offset_bound_and_a_negative_bound_clamps_to_zero() {
    let mut g = waving();
    g.shading.max_vertex_offset = 1.5;
    let compiled = compile_unvalidated(&g).expect("compile");
    let material = lab_material(&g, &compiled, default_params(&g, 0.0), &BTreeMap::new()).expect("material");
    let surface = material.surface_graph.as_ref().expect("surface");
    assert_eq!(surface.max_vertex_offset, 1.5);
    assert!(surface.vertex_wgsl.is_some() && surface.uses_time);
    g.shading.max_vertex_offset = -2.0;
    let material = lab_material(&g, &compiled, default_params(&g, 0.0), &BTreeMap::new()).expect("material");
    assert_eq!(
        material
            .surface_graph
            .as_ref()
            .expect("surface")
            .max_vertex_offset,
        0.0
    );

    // Without a vertex stage the bound is ignored.
    let mut flat = textured();
    flat.shading.max_vertex_offset = 4.0;
    let compiled = compile_unvalidated(&flat).expect("compile");
    let images = BTreeMap::from([(s("albedo"), pixel())]);
    let material = lab_material(&flat, &compiled, default_params(&flat, 0.0), &images).expect("material");
    assert_eq!(
        material
            .surface_graph
            .as_ref()
            .expect("surface")
            .max_vertex_offset,
        0.0
    );
}

#[test]
fn instance_materials_report_instance_errors_and_missing_textures() {
    let g = textured();
    let compiled = compile_unvalidated(&g).expect("compile");
    let mut bad = MaterialInstance::new("p");
    bad.set("gloss", ParamValue::Float(1.0));
    let images = BTreeMap::from([(s("albedo"), pixel())]);
    assert_eq!(
        instance_material(&g, &compiled, &bad, &images, 0.0).expect_err("unknown param"),
        MaterialBuildError::Instance(InstanceError::UnknownParam(s("gloss")))
    );
    // The instance is validated before the textures are looked up.
    assert_eq!(
        instance_material(&g, &compiled, &bad, &BTreeMap::new(), 0.0).expect_err("unknown param"),
        MaterialBuildError::Instance(InstanceError::UnknownParam(s("gloss")))
    );
    assert_eq!(
        instance_material(&g, &compiled, &MaterialInstance::new("p"), &BTreeMap::new(), 0.0)
            .expect_err("no image"),
        MaterialBuildError::MissingTexture(s("albedo"))
    );
}
