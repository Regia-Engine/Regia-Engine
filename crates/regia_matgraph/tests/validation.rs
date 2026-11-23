// SPDX-License-Identifier: MPL-2.0
//! Compile-time validation of material graphs through the public API: every error path that the
//! type checker, the name/limit checks, the vertex-stage rules and lowering (switches and
//! functions) report, and the node each error names. No GPU: graphs go through
//! `compile_unvalidated`.

use std::collections::BTreeMap;

use regia_matgraph::{
    Channel, ColorSpace, CompileError, FunctionInput, FunctionSource, MATGRAPH_FORMAT_VERSION,
    MaterialFunction, MaterialGraph, Node, NormalBlend, ParamDecl, ParamValue, SwitchDecl, TextureDecl,
    ValueType, compile_unvalidated, compile_unvalidated_with,
};

/// A graph with a texture `tex`, a typed constant per width and the first UV set, plus `extra`.
fn base(extra: &[(&str, Node)]) -> MaterialGraph {
    let mut g = MaterialGraph::new("validation");
    g.textures.push(TextureDecl {
        name: "tex".into(),
        path: String::new(),
        color_space: ColorSpace::Linear,
    });
    let fixed = [
        ("f", Node::Float(1.0)),
        ("v2", Node::Vec2([0.5, 0.5])),
        ("v3", Node::Vec3([0.5, 0.5, 0.5])),
        ("v4", Node::Vec4([0.5, 0.5, 0.5, 1.0])),
        ("uv", Node::TexCoord),
    ];
    for (name, node) in fixed {
        g.nodes.insert(name.into(), node);
    }
    for (name, node) in extra {
        g.nodes.insert((*name).into(), node.clone());
    }
    g
}

/// The errors of the graph whose node `t` (the node under test) drives `emissive`.
fn errors_of(extra: &[(&str, Node)]) -> Vec<CompileError> {
    let mut g = base(extra);
    g.output.emissive = Some("t".into());
    compile_unvalidated(&g)
        .expect_err("the graph should not compile")
        .0
}

fn has(errors: &[CompileError], node: &str, needle: &str) -> bool {
    errors
        .iter()
        .any(|e| e.node.as_deref() == Some(node) && e.message.contains(needle))
}

fn s(text: &str) -> String {
    text.to_string()
}

#[test]
fn an_empty_graph_compiles_to_the_default_surface() {
    let compiled = compile_unvalidated(&MaterialGraph::new("empty")).expect("compile");
    assert!(compiled.wgsl.contains("fn material_surface"));
    assert!(compiled.wgsl.contains("vec3<f32>(0.8)"), "default base colour");
    assert!(compiled.wgsl.contains("0.5"), "default roughness");
    assert!(compiled.vertex_wgsl.is_none());
    assert!(compiled.params.is_empty() && compiled.textures.is_empty());
    assert!(!compiled.uses_time && !compiled.uses_uv1);
}

#[test]
fn a_stale_format_version_is_rejected_with_a_migration_hint() {
    let mut g = MaterialGraph::new("old");
    g.format_version = 1;
    let errors = compile_unvalidated(&g).expect_err("v1 must be migrated first").0;
    let hint = format!("migrate to v{MATGRAPH_FORMAT_VERSION}");
    assert!(
        errors
            .iter()
            .any(|e| e.node.is_none() && e.message.contains(&hint)),
        "{errors:?}"
    );
}

#[test]
fn names_are_one_to_sixty_four_identifier_characters() {
    let bad_names = [s(""), s("has space"), s("dash-ed"), s("ünï"), "n".repeat(65)];
    for bad in &bad_names {
        let mut g = base(&[]);
        g.nodes.insert(bad.clone(), Node::Float(0.0));
        let errors = compile_unvalidated(&g).expect_err("bad node name").0;
        assert!(has(&errors, bad, "node names are 1–64"), "{bad:?}: {errors:?}");
    }
    let mut g = base(&[("ok_Name_09", Node::Float(0.0))]);
    g.nodes.insert("n".repeat(64), Node::Float(0.0));
    g.output.roughness = Some("ok_Name_09".into());
    assert!(compile_unvalidated(&g).is_ok());
}

#[test]
fn parameter_and_texture_declarations_are_checked() {
    let mut g = base(&[]);
    g.params.push(ParamDecl::new("dup", ParamValue::Float(0.0)));
    g.params.push(ParamDecl::new("dup", ParamValue::Float(1.0)));
    g.params.push(ParamDecl::new("bad name", ParamValue::Float(0.0)));
    g.params
        .push(ParamDecl::new("inf", ParamValue::Vec2([0.0, f32::INFINITY])));
    g.textures.push(TextureDecl {
        name: "tex".into(),
        path: String::new(),
        color_space: ColorSpace::Srgb,
    });
    let errors = compile_unvalidated(&g).expect_err("declarations").0;
    assert!(has(&errors, "param:dup", "declared twice"), "{errors:?}");
    assert!(has(&errors, "param:bad name", "parameter names"), "{errors:?}");
    assert!(has(&errors, "param:inf", "default is not finite"), "{errors:?}");
    assert!(has(&errors, "texture:tex", "declared twice"), "{errors:?}");
}

#[test]
fn texture_slots_are_limited() {
    let mut g = base(&[]);
    let slots = regia_render::surface_graph::GRAPH_TEXTURE_SLOTS;
    for i in 0..=slots {
        g.textures.push(TextureDecl {
            name: format!("extra{i}"),
            path: String::new(),
            color_space: ColorSpace::Linear,
        });
    }
    let errors = compile_unvalidated(&g).expect_err("too many textures").0;
    assert!(
        errors
            .iter()
            .any(|e| e.node.is_none() && e.message.contains(&format!("at most {slots}"))),
        "{errors:?}"
    );
}

#[test]
fn references_to_undeclared_parameters_and_textures_name_the_node() {
    let errors = errors_of(&[("t", Node::Param(s("ghost")))]);
    assert!(has(&errors, "t", "unknown parameter `ghost`"), "{errors:?}");
    let errors = errors_of(&[(
        "t",
        Node::Sample {
            texture: s("ghost"),
            uv: s("uv"),
            lod: None,
        },
    )]);
    assert!(has(&errors, "t", "unknown texture `ghost`"), "{errors:?}");
    let errors = errors_of(&[(
        "t",
        Node::Triplanar {
            texture: s("ghost"),
            scale: s("f"),
            sharpness: 4.0,
        },
    )]);
    assert!(has(&errors, "t", "unknown texture `ghost`"), "{errors:?}");
}

#[test]
fn sampling_and_projection_inputs_are_typed() {
    let sample = |uv: &str, lod: Option<&str>| Node::Sample {
        texture: s("tex"),
        uv: s(uv),
        lod: lod.map(s),
    };
    let errors = errors_of(&[("t", sample("v3", None))]);
    assert!(has(&errors, "t", "uv must be Vec2"), "{errors:?}");
    let errors = errors_of(&[("t", sample("uv", Some("v2")))]);
    assert!(has(&errors, "t", "lod must be Float"), "{errors:?}");

    let tri = |scale: &str, sharpness: f32| Node::Triplanar {
        texture: s("tex"),
        scale: s(scale),
        sharpness,
    };
    let errors = errors_of(&[("t", tri("v2", 4.0))]);
    assert!(has(&errors, "t", "scale must be Float"), "{errors:?}");
    for sharpness in [0.5, f32::NAN, f32::INFINITY] {
        let errors = errors_of(&[("t", tri("f", sharpness))]);
        assert!(has(&errors, "t", "sharpness must be"), "{sharpness}: {errors:?}");
    }
    // Exactly 1 is the lowest accepted sharpness.
    let mut g = base(&[("t", tri("f", 1.0))]);
    g.output.base_color = Some("tv".into());
    g.nodes.insert(
        "tv".into(),
        Node::Swizzle {
            input: s("t"),
            channels: s("rgb"),
        },
    );
    assert!(compile_unvalidated(&g).is_ok());
}

#[test]
fn uv_transform_and_parallax_check_their_constants() {
    let transform = |scale: [f32; 2], offset: [f32; 2], rotation: f32| Node::UvTransform {
        uv: s("uv"),
        scale,
        offset,
        rotation,
    };
    for node in [
        transform([1.0, 1.0], [0.0, 0.0], f32::NAN),
        transform([f32::INFINITY, 1.0], [0.0, 0.0], 0.0),
        transform([1.0, 1.0], [0.0, f32::NEG_INFINITY], 0.0),
    ] {
        let errors = errors_of(&[("t", node)]);
        assert!(has(&errors, "t", "constant is not finite"), "{errors:?}");
    }
    let errors = errors_of(&[(
        "t",
        Node::UvTransform {
            uv: s("v3"),
            scale: [1.0, 1.0],
            offset: [0.0, 0.0],
            rotation: 0.0,
        },
    )]);
    assert!(has(&errors, "t", "uv must be Vec2"), "{errors:?}");

    let parallax = |uv: &str, scale: &str, min: f32, max: f32| Node::Parallax {
        texture: s("tex"),
        channel: Channel::G,
        uv: s(uv),
        scale: s(scale),
        min_steps: min,
        max_steps: max,
    };
    let errors = errors_of(&[("t", parallax("v3", "f", 2.0, 8.0))]);
    assert!(has(&errors, "t", "uv must be Vec2"), "{errors:?}");
    let errors = errors_of(&[("t", parallax("uv", "v2", 2.0, 8.0))]);
    assert!(has(&errors, "t", "scale must be Float"), "{errors:?}");
    for (min, max) in [(0.5, 8.0), (2.0, 0.0), (f32::NAN, 8.0), (2.0, f32::INFINITY)] {
        let errors = errors_of(&[("t", parallax("uv", "f", min, max))]);
        assert!(has(&errors, "t", "steps must be"), "{min} {max}: {errors:?}");
    }
    // In-order step counts compile, and the helper still clamps each constant (max to 1..16, min
    // to 1..max). Min above max was accepted here until 7cd642a48 made it a compile error on the
    // node; `parallax_min_steps_above_max_steps_is_rejected` covers that (ADR-7386).
    let mut g = base(&[
        ("t", parallax("uv", "f", 4.0, 8.0)),
        (
            "s",
            Node::Sample {
                texture: s("tex"),
                uv: s("t"),
                lod: None,
            },
        ),
        (
            "rgb",
            Node::Swizzle {
                input: s("s"),
                channels: s("rgb"),
            },
        ),
    ]);
    g.output.emissive = Some("rgb".into());
    let wgsl = compile_unvalidated(&g).expect("in-order steps compile").wgsl;
    assert!(
        wgsl.contains("clamp(8, 1.0, 16.0)") || wgsl.contains("clamp(8.0, 1.0, 16.0)"),
        "{wgsl}"
    );
    assert!(
        wgsl.contains("clamp(4, 1.0, max_steps)") || wgsl.contains("clamp(4.0, 1.0, max_steps)"),
        "{wgsl}"
    );
}

/// `min_steps` above `max_steps` is a compile error on the node. The height result is swizzled
/// to `alpha`, so a type mismatch cannot hide a missing check.
#[test]
fn parallax_min_steps_above_max_steps_is_rejected() {
    let parallax = |min: f32, max: f32| Node::Parallax {
        texture: s("tex"),
        channel: Channel::G,
        uv: s("uv"),
        scale: s("f"),
        min_steps: min,
        max_steps: max,
    };
    let graph = |min: f32, max: f32| {
        let mut g = base(&[
            ("t", parallax(min, max)),
            (
                "x",
                Node::Swizzle {
                    input: s("t"),
                    channels: s("x"),
                },
            ),
        ]);
        g.output.alpha = Some("x".into());
        g
    };
    for (min, max) in [(8.0, 2.0), (16.0, 1.0)] {
        let errors = compile_unvalidated(&graph(min, max))
            .expect_err("min_steps above max_steps")
            .0;
        assert!(
            has(&errors, "t", "min_steps must not exceed max_steps"),
            "{min} {max}: {errors:?}"
        );
    }
    assert!(
        compile_unvalidated(&graph(4.0, 4.0)).is_ok(),
        "equal step counts stay valid"
    );
}

#[test]
fn non_finite_constants_are_rejected_for_every_width() {
    for node in [
        Node::Float(f32::NAN),
        Node::Vec2([0.0, f32::INFINITY]),
        Node::Vec3([f32::NEG_INFINITY, 0.0, 0.0]),
        Node::Vec4([0.0, 0.0, 0.0, f32::NAN]),
    ] {
        let errors = errors_of(&[("t", node)]);
        assert!(has(&errors, "t", "constant is not finite"), "{errors:?}");
    }
}

#[test]
fn arithmetic_needs_matching_widths_or_a_scalar() {
    let pairs: [(&str, Node); 6] = [
        ("Add", Node::Add(s("v2"), s("v3"))),
        ("Sub", Node::Sub(s("v3"), s("v4"))),
        ("Mul", Node::Mul(s("v2"), s("v4"))),
        ("Div", Node::Div(s("v3"), s("v2"))),
        ("Min", Node::Min(s("v2"), s("v3"))),
        ("Pow", Node::Pow(s("v4"), s("v2"))),
    ];
    for (kind, node) in pairs {
        let errors = errors_of(&[("t", node)]);
        assert!(has(&errors, "t", "widths must match"), "{kind}: {errors:?}");
    }
    // A Float broadcasts on either side and the wider side types the result.
    let mut g = base(&[("t", Node::Pow(s("f"), s("v3")))]);
    g.output.emissive = Some("t".into());
    let compiled = compile_unvalidated(&g).expect("float broadcasts");
    assert_eq!(compiled.node_types["t"], ValueType::Vec3);
    let mut g = base(&[("t", Node::Max(s("v4"), s("f")))]);
    g.nodes.insert(
        "rgb".into(),
        Node::Swizzle {
            input: s("t"),
            channels: s("rgb"),
        },
    );
    g.output.emissive = Some("rgb".into());
    assert_eq!(
        compile_unvalidated(&g).expect("compile").node_types["t"],
        ValueType::Vec4
    );
}

#[test]
fn dot_lerp_smoothstep_and_vector_ops_are_typed() {
    for node in [
        Node::Dot(s("v3"), s("v2")),
        Node::Dot(s("f"), s("f")),
        Node::Dot(s("v3"), s("f")),
    ] {
        let errors = errors_of(&[("t", node)]);
        assert!(has(&errors, "t", "Dot needs two vectors"), "{errors:?}");
    }
    let errors = errors_of(&[(
        "t",
        Node::Lerp {
            a: s("v3"),
            b: s("v3"),
            t: s("v2"),
        },
    )]);
    assert!(has(&errors, "t", "t must be Float or Vec3"), "{errors:?}");
    let errors = errors_of(&[(
        "t",
        Node::Lerp {
            a: s("v3"),
            b: s("v2"),
            t: s("f"),
        },
    )]);
    assert!(has(&errors, "t", "widths must match"), "{errors:?}");
    let errors = errors_of(&[(
        "t",
        Node::Smoothstep {
            edge0: s("v2"),
            edge1: s("v3"),
            x: s("f"),
        },
    )]);
    assert!(has(&errors, "t", "widths must match"), "{errors:?}");
    let errors = errors_of(&[(
        "t",
        Node::Smoothstep {
            edge0: s("f"),
            edge1: s("f"),
            x: s("v2"),
        },
    )]);
    // Scalar edges with a vector input are valid; this graph only fails at the output type.
    assert!(!has(&errors, "t", "widths must match"), "{errors:?}");
    for node in [Node::Normalize(s("f")), Node::Length(s("f"))] {
        let errors = errors_of(&[("t", node)]);
        assert!(has(&errors, "t", "needs a vector"), "{errors:?}");
    }
}

#[test]
fn swizzles_check_alphabet_length_and_width() {
    let swizzle = |input: &str, channels: &str| Node::Swizzle {
        input: s(input),
        channels: s(channels),
    };
    for channels in ["", "xq", "xr", "xyzwx", "rgbaa", "X"] {
        let errors = errors_of(&[("t", swizzle("v4", channels))]);
        assert!(has(&errors, "t", "bad swizzle"), "{channels:?}: {errors:?}");
    }
    let errors = errors_of(&[("t", swizzle("v2", "z"))]);
    assert!(has(&errors, "t", "reads past a Vec2"), "{errors:?}");
    let errors = errors_of(&[("t", swizzle("v3", "rgba"))]);
    assert!(has(&errors, "t", "reads past a Vec3"), "{errors:?}");
    // Reordering and repeating components is fine, in either alphabet, up to four.
    for (channels, ty) in [
        ("zyx", ValueType::Vec3),
        ("rrrr", ValueType::Vec4),
        ("x", ValueType::Float),
    ] {
        let mut g = base(&[("t", swizzle("v3", channels))]);
        // Hang the node on an output of its own type so the graph compiles.
        match ty {
            ValueType::Float => g.output.alpha = Some("t".into()),
            ValueType::Vec3 => g.output.emissive = Some("t".into()),
            _ => {
                g.nodes.insert(s("rgb"), swizzle("t", "rgb"));
                g.output.emissive = Some("rgb".into());
            }
        }
        let compiled = compile_unvalidated(&g).expect("valid swizzle");
        assert_eq!(compiled.node_types["t"], ty, "{channels}");
    }
}

#[test]
fn append_cannot_exceed_four_components() {
    let errors = errors_of(&[("t", Node::Append(s("v3"), s("v2")))]);
    assert!(has(&errors, "t", "wider than Vec4"), "{errors:?}");
    let errors = errors_of(&[("t", Node::Append(s("v4"), s("f")))]);
    assert!(has(&errors, "t", "wider than Vec4"), "{errors:?}");
    let mut g = base(&[("t", Node::Append(s("v3"), s("f")))]);
    g.output.emissive = Some("rgb".into());
    g.nodes.insert(
        "rgb".into(),
        Node::Swizzle {
            input: s("t"),
            channels: s("xyz"),
        },
    );
    assert_eq!(
        compile_unvalidated(&g).expect("vec3 + float").node_types["t"],
        ValueType::Vec4
    );
}

#[test]
fn shading_nodes_check_their_inputs() {
    let errors = errors_of(&[("t", Node::Fresnel { power: s("v3") })]);
    assert!(has(&errors, "t", "power must be Float"), "{errors:?}");
    let errors = errors_of(&[("t", Node::UnpackNormal(s("v2")))]);
    assert!(has(&errors, "t", "needs a Vec3 or Vec4"), "{errors:?}");
    let errors = errors_of(&[("t", Node::UnpackNormal(s("f")))]);
    assert!(has(&errors, "t", "needs a Vec3 or Vec4"), "{errors:?}");
    let errors = errors_of(&[(
        "t",
        Node::NormalStrength {
            normal: s("v2"),
            strength: s("f"),
        },
    )]);
    assert!(has(&errors, "t", "normal must be Vec3"), "{errors:?}");
    let errors = errors_of(&[(
        "t",
        Node::NormalStrength {
            normal: s("v3"),
            strength: s("v3"),
        },
    )]);
    assert!(has(&errors, "t", "strength must be Float"), "{errors:?}");
    for mode in [NormalBlend::Udn, NormalBlend::Whiteout, NormalBlend::Rnm] {
        let errors = errors_of(&[(
            "t",
            Node::BlendNormals {
                base: s("v2"),
                detail: s("v3"),
                mode,
            },
        )]);
        assert!(has(&errors, "t", "base must be Vec3"), "{mode:?}: {errors:?}");
        let errors = errors_of(&[(
            "t",
            Node::BlendNormals {
                base: s("v3"),
                detail: s("v4"),
                mode,
            },
        )]);
        assert!(has(&errors, "t", "detail must be Vec3"), "{mode:?}: {errors:?}");
    }
}

#[test]
fn height_blend_needs_float_heights_and_a_positive_depth() {
    let blend = |height_a: &str, height_b: &str, t: &str, depth: f32| Node::HeightBlend {
        a: s("v3"),
        b: s("v3"),
        height_a: s(height_a),
        height_b: s(height_b),
        t: s(t),
        depth,
    };
    let errors = errors_of(&[("t", blend("v3", "f", "f", 0.2))]);
    assert!(has(&errors, "t", "height_a must be Float"), "{errors:?}");
    let errors = errors_of(&[("t", blend("f", "v2", "f", 0.2))]);
    assert!(has(&errors, "t", "height_b must be Float"), "{errors:?}");
    let errors = errors_of(&[("t", blend("f", "f", "v4", 0.2))]);
    assert!(has(&errors, "t", "t must be Float"), "{errors:?}");
    for depth in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        let errors = errors_of(&[("t", blend("f", "f", "f", depth))]);
        assert!(has(&errors, "t", "depth must be > 0"), "{depth}: {errors:?}");
    }
}

#[test]
fn outputs_must_name_a_node_of_the_slot_type() {
    // Each slot takes one type; the error names `output:<slot>` and points at Swizzle.
    let wrong: [(&str, &str); 8] = [
        ("base_color", "f"),
        ("alpha", "v3"),
        ("metallic", "v2"),
        ("roughness", "v4"),
        ("normal", "f"),
        ("occlusion", "v3"),
        ("emissive", "v4"),
        ("world_position_offset", "v2"),
    ];
    for (slot, node) in wrong {
        let mut g = base(&[]);
        let o = &mut g.output;
        let target = Some(node.to_string());
        match slot {
            "base_color" => o.base_color = target,
            "alpha" => o.alpha = target,
            "metallic" => o.metallic = target,
            "roughness" => o.roughness = target,
            "normal" => o.normal = target,
            "occlusion" => o.occlusion = target,
            "emissive" => o.emissive = target,
            _ => o.world_position_offset = target,
        }
        let errors = compile_unvalidated(&g).expect_err(slot).0;
        assert!(
            has(&errors, &format!("output:{slot}"), "(use Swizzle)"),
            "{slot}: {errors:?}"
        );
    }
    let mut g = base(&[]);
    g.output.normal = Some("ghost".into());
    let errors = compile_unvalidated(&g).expect_err("unknown output node").0;
    assert!(
        has(&errors, "output:normal", "unknown node `ghost`"),
        "{errors:?}"
    );
}

#[test]
fn the_vertex_stage_rejects_eye_dependent_and_per_pixel_nodes() {
    let eye_dependent: [(&str, Node); 3] = [
        ("view", Node::ViewDirection),
        ("distance", Node::CameraDistance),
        ("fresnel", Node::Fresnel { power: s("f") }),
    ];
    for (name, node) in eye_dependent {
        let mut g = base(&[(name, node)]);
        g.nodes.insert("wide".into(), Node::Mul(s("v3"), s(name)));
        g.output.world_position_offset = Some("wide".into());
        let errors = compile_unvalidated(&g).expect_err(name).0;
        assert!(has(&errors, name, "is eye-dependent"), "{name}: {errors:?}");
    }
    let mut g = base(&[(
        "march",
        Node::Parallax {
            texture: s("tex"),
            channel: Channel::R,
            uv: s("uv"),
            scale: s("f"),
            min_steps: 1.0,
            max_steps: 4.0,
        },
    )]);
    g.nodes.insert(
        "xy".into(),
        Node::Swizzle {
            input: s("march"),
            channels: s("xy"),
        },
    );
    g.nodes.insert("off".into(), Node::Append(s("xy"), s("f")));
    g.output.world_position_offset = Some("off".into());
    let errors = compile_unvalidated(&g)
        .expect_err("parallax in the vertex stage")
        .0;
    assert!(has(&errors, "march", "per-pixel march"), "{errors:?}");

    // The same eye-dependent node is fine in the fragment stage.
    let mut g = base(&[("t", Node::Fresnel { power: s("f") })]);
    g.nodes.insert("c".into(), Node::Mul(s("v3"), s("t")));
    g.output.emissive = Some("c".into());
    assert!(compile_unvalidated(&g).is_ok());
}

#[test]
fn a_time_driven_vertex_offset_compiles_and_flags_time() {
    let mut g = base(&[("time", Node::Time), ("wave", Node::Sin(s("time")))]);
    g.nodes.insert("off".into(), Node::Mul(s("v3"), s("wave")));
    g.output.world_position_offset = Some("off".into());
    let compiled = compile_unvalidated(&g).expect("compile");
    assert!(compiled.uses_time);
    assert!(compiled.vertex_wgsl.is_some());
    assert_eq!(compiled.node_types["wave"], ValueType::Float);
}

#[test]
fn trig_and_frac_keep_the_input_width() {
    for node in [Node::Sin(s("v3")), Node::Cos(s("v3")), Node::Frac(s("v3"))] {
        let mut g = base(&[("t", node)]);
        g.output.emissive = Some("t".into());
        assert_eq!(
            compile_unvalidated(&g).expect("compile").node_types["t"],
            ValueType::Vec3
        );
    }
}

// -- Lowering: switches and functions --------------------------------------------------------

fn halve() -> MaterialFunction {
    let mut nodes = BTreeMap::new();
    nodes.insert(s("x"), Node::Input(s("x")));
    nodes.insert(s("k"), Node::Float(0.5));
    nodes.insert(s("half"), Node::Mul(s("x"), s("k")));
    let mut outputs = BTreeMap::new();
    outputs.insert(s("out"), s("half"));
    MaterialFunction {
        format_version: regia_matgraph::MATFUNC_FORMAT_VERSION,
        name: s("halve"),
        inputs: vec![FunctionInput {
            name: s("x"),
            ty: ValueType::Vec3,
        }],
        nodes,
        outputs,
    }
}

fn call(function: &str, args: &[(&str, &str)], output: Option<&str>) -> Node {
    Node::Call {
        function: s(function),
        args: args.iter().map(|(k, v)| (s(k), s(v))).collect(),
        output: output.map(s),
    }
}

/// A function with two inputs where each output reads only one of them.
fn two_inputs() -> MaterialFunction {
    let mut f = halve();
    f.inputs.push(FunctionInput {
        name: s("amount"),
        ty: ValueType::Float,
    });
    f.nodes.insert(s("amount"), Node::Input(s("amount")));
    f.nodes.insert(s("k2"), Node::Float(0.5));
    f.nodes.insert(s("scaled"), Node::Mul(s("amount"), s("k2")));
    f.outputs.insert(s("scaled"), s("scaled"));
    f
}

#[test]
fn an_argument_the_chosen_output_never_reads_is_still_type_checked() {
    let mut g = base(&[]);
    g.functions.insert(s("two"), FunctionSource::Inline(two_inputs()));
    // `out` reads only `x`; `amount` (Float) is given a Vec2.
    g.nodes
        .insert(s("t"), call("two", &[("x", "v3"), ("amount", "v2")], Some("out")));
    g.output.emissive = Some("t".into());
    let errors = compile_unvalidated(&g).expect_err("bad unread argument").0;
    assert!(
        errors.iter().any(|e| e
            .message
            .contains("argument `amount` is Vec2; the function takes Float")),
        "{errors:?}"
    );
}

#[test]
fn function_calls_are_checked_for_arguments_outputs_and_types() {
    let cases = [
        (
            call("halve", &[("x", "v3"), ("y", "f")], None),
            "`halve` has no input `y`",
        ),
        (
            call("halve", &[("x", "v2")], None),
            "argument `x` is Vec2; the function takes Vec3",
        ),
        (call("nope", &[], None), "unknown function `nope`"),
        (call("halve", &[], None), "missing argument `x`"),
        (
            call("halve", &[("x", "v3")], Some("ghost")),
            "`halve` has no output `ghost`",
        ),
    ];
    for (node, wanted) in cases {
        let mut g = base(&[]);
        g.functions.insert(s("halve"), FunctionSource::Inline(halve()));
        g.nodes.insert(s("t"), node);
        g.output.emissive = Some("t".into());
        let errors = compile_unvalidated(&g).expect_err(wanted).0;
        assert!(has(&errors, "t", wanted), "{wanted}: {errors:?}");
    }
    let mut g = base(&[]);
    g.functions.insert(s("halve"), FunctionSource::Inline(halve()));
    g.nodes
        .insert(s("good"), call("halve", &[("x", "v3")], Some("out")));
    g.output.emissive = Some("good".into());
    let compiled = compile_unvalidated(&g).expect("a well-formed call compiles");
    assert_eq!(compiled.node_types["good/half"], ValueType::Vec3);
}

#[test]
fn a_call_with_several_outputs_must_name_one() {
    let mut f = halve();
    f.outputs.insert(s("second"), s("k"));
    let mut g = base(&[]);
    g.functions.insert(s("halve"), FunctionSource::Inline(f));
    g.nodes
        .insert(s("ambiguous"), call("halve", &[("x", "v3")], None));
    g.output.emissive = Some("ambiguous".into());
    let errors = compile_unvalidated(&g).expect_err("ambiguous output").0;
    assert!(has(&errors, "ambiguous", "has 2 outputs; name one"), "{errors:?}");
    g.nodes
        .insert(s("ambiguous"), call("halve", &[("x", "v3")], Some("out")));
    assert!(compile_unvalidated(&g).is_ok());
}

#[test]
fn an_unresolved_function_file_is_a_compile_error() {
    let mut g = base(&[]);
    g.functions
        .insert(s("halve"), FunctionSource::File(s("fn/halve.matfunc.ron")));
    g.nodes.insert(s("c"), call("halve", &[("x", "v3")], None));
    g.output.emissive = Some("c".into());
    let errors = compile_unvalidated(&g).expect_err("unresolved").0;
    assert!(has(&errors, "c", "was not resolved"), "{errors:?}");
    assert!(has(&errors, "c", "fn/halve.matfunc.ron"), "{errors:?}");
}

#[test]
fn function_and_switch_names_are_validated() {
    let mut f = halve();
    f.nodes.insert(s("bad name"), Node::Float(0.0));
    let mut g = base(&[]);
    g.functions.insert(s("bad-fn"), FunctionSource::Inline(halve()));
    g.functions.insert(s("ok_fn"), FunctionSource::Inline(f));
    g.switches.push(SwitchDecl {
        name: s("bad switch"),
        default: true,
    });
    g.switches.push(SwitchDecl {
        name: s("dup"),
        default: true,
    });
    g.switches.push(SwitchDecl {
        name: s("dup"),
        default: false,
    });
    let errors = compile_unvalidated(&g).expect_err("names").0;
    assert!(has(&errors, "function:bad-fn", "function names"), "{errors:?}");
    assert!(
        has(&errors, "function:ok_fn/bad name", "node names"),
        "{errors:?}"
    );
    assert!(has(&errors, "switch:bad switch", "switch names"), "{errors:?}");
    assert!(has(&errors, "switch:dup", "declared twice"), "{errors:?}");
}

#[test]
fn nesting_deeper_than_eight_functions_is_an_error_and_eight_is_fine() {
    // f0 calls f1 calls ... fN; `depth` functions are nested when the root calls f0.
    fn chain(depth: usize) -> MaterialGraph {
        let mut g = base(&[]);
        for i in 0..depth {
            let mut f = halve();
            if i + 1 < depth {
                f.nodes
                    .insert(s("inner"), call(&format!("f{}", i + 1), &[("x", "x")], None));
                f.outputs.insert(s("out"), s("inner"));
            }
            g.functions.insert(format!("f{i}"), FunctionSource::Inline(f));
        }
        g.nodes.insert(s("top"), call("f0", &[("x", "v3")], None));
        g.output.emissive = Some("top".into());
        g
    }
    assert!(compile_unvalidated(&chain(8)).is_ok());
    let errors = compile_unvalidated(&chain(9)).expect_err("nine deep").0;
    assert!(
        errors.iter().any(|e| e.message.contains("nests deeper than 8")),
        "{errors:?}"
    );
}

#[test]
fn switch_overrides_pick_the_emitted_branch() {
    let mut g = base(&[
        ("red", Node::Vec3([1.0, 0.0, 0.0])),
        ("blue", Node::Vec3([0.0, 0.0, 1.0])),
        (
            "pick",
            Node::Switch {
                switch: s("warm"),
                on: s("red"),
                off: s("blue"),
            },
        ),
    ]);
    g.switches.push(SwitchDecl {
        name: s("warm"),
        default: true,
    });
    g.output.base_color = Some("pick".into());
    let on = compile_unvalidated(&g).expect("default");
    assert_eq!(on.switches.get("warm"), Some(&true));
    assert!(on.wgsl.contains("vec3<f32>(1.0, 0.0, 0.0)"), "{}", on.wgsl);
    assert!(!on.wgsl.contains("vec3<f32>(0.0, 0.0, 1.0)"), "{}", on.wgsl);
    let mut off_map = BTreeMap::new();
    off_map.insert(s("warm"), false);
    let off = compile_unvalidated_with(&g, &off_map).expect("off");
    assert_eq!(off.switches.get("warm"), Some(&false));
    assert!(off.wgsl.contains("vec3<f32>(0.0, 0.0, 1.0)"), "{}", off.wgsl);
    assert!(!off.wgsl.contains("vec3<f32>(1.0, 0.0, 0.0)"), "{}", off.wgsl);
    assert_ne!(on.key, off.key);
    assert_ne!(on.graph_hash, off.graph_hash);
}

#[test]
fn a_switch_cycle_is_reported_instead_of_hanging() {
    let mut g = base(&[
        (
            "a",
            Node::Switch {
                switch: s("s"),
                on: s("b"),
                off: s("b"),
            },
        ),
        (
            "b",
            Node::Switch {
                switch: s("s"),
                on: s("a"),
                off: s("a"),
            },
        ),
        ("t", Node::Saturate(s("a"))),
    ]);
    g.switches.push(SwitchDecl {
        name: s("s"),
        default: true,
    });
    g.output.emissive = Some("t".into());
    let errors = compile_unvalidated(&g).expect_err("cycle").0;
    assert!(errors.iter().any(|e| e.message.contains("cycle")), "{errors:?}");
}

#[test]
fn compiled_slots_follow_declaration_order() {
    let mut g = base(&[]);
    g.params.push(ParamDecl::new(s("a"), ParamValue::Float(0.0)));
    g.params.push(ParamDecl::new(s("b"), ParamValue::Color([0.0; 3])));
    g.textures.push(TextureDecl {
        name: s("second"),
        path: s("x.png"),
        color_space: ColorSpace::Srgb,
    });
    g.nodes.insert(s("pa"), Node::Param(s("a")));
    g.output.roughness = Some("pa".into());
    let compiled = compile_unvalidated(&g).expect("compile");
    // Slot 0 of the parameter block is the host's built-ins.
    assert_eq!(
        compiled.param("a").map(|p| (p.slot, p.ty)),
        Some((1, ValueType::Float))
    );
    assert_eq!(
        compiled.param("b").map(|p| (p.slot, p.ty)),
        Some((2, ValueType::Vec3))
    );
    assert!(compiled.param("missing").is_none());
    let names: Vec<_> = compiled
        .textures
        .iter()
        .map(|t| (t.name.as_str(), t.slot))
        .collect();
    assert_eq!(names, vec![("tex", 0), ("second", 1)]);
    assert_eq!(compiled.textures[1].color_space, ColorSpace::Srgb);
}

fn diagnostic_stress_graph(reverse_insertions: bool) -> MaterialGraph {
    let mut graph = base(&[]);
    let mut invalid_nodes = vec![
        ("bad_add", Node::Add(s("missing_left"), s("missing_right"))),
        ("bad_dot", Node::Dot(s("v3"), s("v2"))),
        ("nested", call("cycle_a", &[("x", "v3")], None)),
    ];
    if reverse_insertions {
        invalid_nodes.reverse();
    }
    for (name, node) in invalid_nodes {
        graph.nodes.insert(s(name), node);
    }
    graph.output.emissive = Some(s("bad_add"));
    graph.output.normal = Some(s("bad_dot"));
    graph.output.roughness = Some(s("nested"));

    let function = |next: &str| {
        let mut nodes = BTreeMap::new();
        nodes.insert(s("arg"), Node::Input(s("x")));
        nodes.insert(s("again"), call(next, &[("x", "arg")], None));
        let mut outputs = BTreeMap::new();
        outputs.insert(s("result"), s("again"));
        MaterialFunction {
            format_version: regia_matgraph::MATFUNC_FORMAT_VERSION,
            name: s(next),
            inputs: vec![FunctionInput {
                name: s("x"),
                ty: ValueType::Vec3,
            }],
            nodes,
            outputs,
        }
    };
    let mut functions = vec![
        ("cycle_a", FunctionSource::Inline(function("cycle_b"))),
        ("cycle_b", FunctionSource::Inline(function("cycle_a"))),
    ];
    if reverse_insertions {
        functions.reverse();
    }
    for (name, function) in functions {
        graph.functions.insert(s(name), function);
    }
    graph
}

#[test]
fn simultaneous_diagnostics_have_stable_identity_and_order() {
    let forward = diagnostic_stress_graph(false);
    let reverse = diagnostic_stress_graph(true);
    let forward_errors = compile_unvalidated(&forward).expect_err("invalid graph").0;
    let reverse_errors = compile_unvalidated(&reverse).expect_err("same invalid graph").0;
    let identity = |errors: &[CompileError]| {
        errors
            .iter()
            .map(|error| (error.node.clone(), error.message.clone()))
            .collect::<Vec<_>>()
    };
    assert!(
        has(&forward_errors, "bad_add", "missing_left"),
        "{forward_errors:?}"
    );
    assert!(
        has(&forward_errors, "bad_add", "missing_right"),
        "{forward_errors:?}"
    );
    assert!(
        has(&forward_errors, "bad_dot", "Dot needs two vectors"),
        "{forward_errors:?}"
    );
    assert!(
        forward_errors
            .iter()
            .any(|error| error.message.contains("calls itself")),
        "{forward_errors:?}"
    );
    assert_eq!(identity(&forward_errors), identity(&reverse_errors));
    for _ in 0..4 {
        let repeated = compile_unvalidated(&forward).expect_err("invalid graph").0;
        assert_eq!(identity(&forward_errors), identity(&repeated));
    }
}

#[test]
fn failed_compiles_do_not_poison_reused_library_or_cache() {
    use regia_matgraph::{MaterialGraphLibrary, ShaderCache};

    let mut library = MaterialGraphLibrary::new();
    let mut cache = ShaderCache::new();
    let bad = diagnostic_stress_graph(false);
    library.set("shared", bad.clone());
    assert!(library.compile("shared").is_some_and(|result| result.is_err()));
    assert!(cache.get_or_compile(&bad).is_err());

    let mut valid = base(&[("color", Node::Vec3([0.25, 0.5, 0.75]))]);
    valid.output.base_color = Some(s("color"));
    library.set("shared", valid.clone());
    let from_library = library
        .compile("shared")
        .expect("library entry")
        .expect("valid after failure")
        .0;
    let from_cache = cache.get_or_compile(&valid).expect("valid after failure").0;
    let from_fresh = ShaderCache::new()
        .get_or_compile(&valid)
        .expect("fresh compile")
        .0;
    assert_eq!(*from_library, *from_fresh);
    assert_eq!(*from_cache, *from_fresh);
    assert_eq!(from_library.wgsl.as_bytes(), from_fresh.wgsl.as_bytes());
    assert_eq!(from_library.vertex_wgsl, from_fresh.vertex_wgsl);
    assert_eq!(from_library.params, from_fresh.params);
    assert_eq!(from_library.textures, from_fresh.textures);
    assert_eq!(from_library.node_types, from_fresh.node_types);
}
