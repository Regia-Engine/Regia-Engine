// SPDX-License-Identifier: MPL-2.0
//! Anim graph validation, loading and migration at their boundaries: every reference check
//! fires, every number is checked for finiteness, versions below, at and above the current one,
//! and failing or missing migration steps. Graphs are built in code and round-tripped through
//! RON where the text matters.

use regia_animation::RootMotionMode;
use regia_animation::blendspace::{BlendSample1D, BlendSample2D, BlendSpace1D, BlendSpace2D};
use regia_animation::graph::{
    ANIMGRAPH_FORMAT_VERSION, AdditiveNode, AnimGraph, AnimGraphDocument, AnimGraphError, AnimNode,
    BlendNode, BlendSpace1DNode, BlendSpace2DNode, ClipNode, IkTarget, JointMaskDef, LayerEntry, LayerMode,
    LayerNode, LookAtNode, MigrationStep, RootMotionNode, TwoBoneIKNode, migrate_document, migrate_with,
    registered_migrations, trigger_decl,
};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::state_machine::{
    CmpOp, Condition, StateMachineDef, TransitionBlend, TransitionCurve, TransitionDef,
};

fn clip(name: &str) -> AnimNode {
    AnimNode::Clip(ClipNode {
        clip: name.into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    })
}

fn graph(root: AnimNode, params: &[&str]) -> AnimGraph {
    AnimGraph {
        parameters: params
            .iter()
            .map(|p| ParamDecl((*p).into(), ParamValue::Float(0.0)))
            .collect(),
        root,
    }
}

fn invalid(graph: &AnimGraph) -> String {
    match graph.validate() {
        Err(AnimGraphError::Invalid(message)) => message,
        other => panic!("expected an Invalid error, got {other:?}"),
    }
}

fn transition(from: Option<&str>, to: &str) -> TransitionDef {
    TransitionDef {
        from: from.map(String::from),
        to: to.into(),
        conditions: vec![],
        duration: 0.2,
        curve: TransitionCurve::Linear,
        interruptible: true,
        blend: TransitionBlend::Crossfade,
    }
}

fn machine(transitions: Vec<TransitionDef>) -> AnimNode {
    AnimNode::StateMachine(StateMachineDef {
        initial: "idle".into(),
        states: vec![("idle".into(), clip("idle")), ("run".into(), clip("run"))],
        transitions,
    })
}

#[test]
fn a_well_formed_graph_validates_and_triangulating_is_idempotent() {
    let g = graph(
        machine(vec![transition(Some("idle"), "run"), transition(None, "idle")]),
        &[],
    );
    assert!(g.validate().is_ok());
    let space = BlendSpace2D {
        param_x: "x".into(),
        param_y: "y".into(),
        samples: [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]
            .into_iter()
            .map(|point| BlendSample2D {
                point,
                clip: "c".into(),
            })
            .collect(),
        triangles: vec![],
    };
    let g2 = graph(
        AnimNode::BlendSpace2D(BlendSpace2DNode {
            space,
            sync_group: None,
        }),
        &["x", "y"],
    );
    let once = g2.triangulated();
    assert_eq!(once.triangulated(), once);
    let AnimNode::BlendSpace2D(n) = &once.root else {
        panic!("kept the node")
    };
    assert_eq!(n.space.triangles.len(), 1);
}

#[test]
fn every_parameter_reference_is_checked() {
    let blend = |param: &str| {
        graph(
            AnimNode::Blend(BlendNode {
                a: Box::new(clip("a")),
                b: Box::new(clip("b")),
                alpha_param: param.into(),
            }),
            &["ok"],
        )
    };
    assert!(blend("ok").validate().is_ok());
    assert!(invalid(&blend("nope")).contains("nope"));

    let additive = |param: &str| {
        graph(
            AnimNode::Additive(AdditiveNode {
                base: Box::new(clip("a")),
                additive: Box::new(clip("b")),
                weight_param: param.into(),
            }),
            &["ok"],
        )
    };
    assert!(additive("ok").validate().is_ok());
    assert!(invalid(&additive("nope")).contains("additive"));

    let layer = |param: &str, inertialize: f32| {
        graph(
            AnimNode::Layer(LayerNode {
                base: Box::new(clip("base")),
                layers: vec![LayerEntry {
                    graph: Box::new(clip("l")),
                    mask: JointMaskDef {
                        joints: vec![],
                        include_children: true,
                    },
                    mode: LayerMode::Override,
                    weight_param: param.into(),
                    weight_inertialize: inertialize,
                }],
            }),
            &["ok"],
        )
    };
    assert!(layer("ok", 0.0).validate().is_ok());
    assert!(invalid(&layer("nope", 0.0)).contains("layer"));
    assert!(invalid(&layer("ok", f32::NAN)).contains("weight_inertialize"));
    assert!(invalid(&layer("ok", f32::INFINITY)).contains("weight_inertialize"));

    let ik = |target: IkTarget, pole: Option<&str>, weight: Option<&str>| {
        graph(
            AnimNode::TwoBoneIK(TwoBoneIKNode {
                inner: Box::new(clip("a")),
                chain: ["a".into(), "b".into(), "c".into()],
                target,
                pole: pole.map(String::from),
                weight_param: weight.map(String::from),
            }),
            &["ok"],
        )
    };
    assert!(
        ik(IkTarget::Param("ok".into()), Some("ok"), Some("ok"))
            .validate()
            .is_ok()
    );
    assert!(
        ik(IkTarget::Socket("anything".into()), None, None)
            .validate()
            .is_ok(),
        "sockets are not parameters"
    );
    assert!(invalid(&ik(IkTarget::Param("nope".into()), None, None)).contains("target"));
    assert!(invalid(&ik(IkTarget::Socket("s".into()), Some("nope"), None)).contains("pole"));
    assert!(invalid(&ik(IkTarget::Socket("s".into()), None, Some("nope"))).contains("weight"));

    let look = |target: IkTarget, weight: Option<&str>, yaw: f32, pitch: f32| {
        graph(
            AnimNode::LookAt(LookAtNode {
                inner: Box::new(clip("a")),
                joint: "head".into(),
                target,
                max_yaw: yaw,
                max_pitch: pitch,
                weight_param: weight.map(String::from),
            }),
            &["ok"],
        )
    };
    assert!(
        look(IkTarget::Param("ok".into()), Some("ok"), 1.0, 1.0)
            .validate()
            .is_ok()
    );
    assert!(invalid(&look(IkTarget::Param("nope".into()), None, 1.0, 1.0)).contains("target"));
    assert!(invalid(&look(IkTarget::Socket("s".into()), Some("nope"), 1.0, 1.0)).contains("weight"));
    assert!(invalid(&look(IkTarget::Socket("s".into()), None, f32::NAN, 1.0)).contains("max_yaw"));
    assert!(invalid(&look(IkTarget::Socket("s".into()), None, 1.0, f32::INFINITY)).contains("max_pitch"));
}

#[test]
fn nested_nodes_are_validated_through_wrappers() {
    let bad = clip("x");
    let AnimNode::Clip(mut c) = bad else {
        unreachable!()
    };
    c.speed = f32::NAN;
    let nan_clip = AnimNode::Clip(c);
    let wrapped = graph(
        AnimNode::RootMotion(RootMotionNode {
            inner: Box::new(nan_clip.clone()),
            mode: RootMotionMode::XZ,
        }),
        &[],
    );
    assert!(invalid(&wrapped).contains("speed"));
    let deep = graph(
        AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(AnimNode::LookAt(LookAtNode {
                inner: Box::new(nan_clip),
                joint: "j".into(),
                target: IkTarget::Socket("s".into()),
                max_yaw: 1.0,
                max_pitch: 1.0,
                weight_param: None,
            })),
            chain: ["a".into(), "b".into(), "c".into()],
            target: IkTarget::Socket("s".into()),
            pole: None,
            weight_param: None,
        }),
        &[],
    );
    assert!(invalid(&deep).contains("speed"));
    // A state's inner node is validated too.
    let sm = graph(
        AnimNode::StateMachine(StateMachineDef {
            initial: "a".into(),
            states: vec![(
                "a".into(),
                AnimNode::Blend(BlendNode {
                    a: Box::new(clip("x")),
                    b: Box::new(clip("y")),
                    alpha_param: "missing".into(),
                }),
            )],
            transitions: vec![],
        }),
        &[],
    );
    assert!(invalid(&sm).contains("missing"));
}

#[test]
fn blend_spaces_need_samples_and_finite_positions() {
    let one_d = |samples: Vec<f32>| {
        graph(
            AnimNode::BlendSpace1D(BlendSpace1DNode {
                space: BlendSpace1D {
                    param: "p".into(),
                    samples: samples
                        .into_iter()
                        .map(|value| BlendSample1D {
                            value,
                            clip: "c".into(),
                        })
                        .collect(),
                },
                sync_group: None,
            }),
            &["p"],
        )
    };
    assert!(one_d(vec![0.0, 1.0]).validate().is_ok());
    assert!(invalid(&one_d(vec![])).contains("no samples"));
    assert!(invalid(&one_d(vec![0.0, f32::NAN])).contains("finite"));
    let two_d = |x: &str, y: &str, point: [f32; 2], empty: bool| {
        graph(
            AnimNode::BlendSpace2D(BlendSpace2DNode {
                space: BlendSpace2D {
                    param_x: x.into(),
                    param_y: y.into(),
                    samples: if empty {
                        vec![]
                    } else {
                        vec![BlendSample2D {
                            point,
                            clip: "c".into(),
                        }]
                    },
                    triangles: vec![],
                },
                sync_group: None,
            }),
            &["x", "y"],
        )
    };
    assert!(two_d("x", "y", [0.0, 0.0], false).validate().is_ok());
    assert!(invalid(&two_d("nope", "y", [0.0, 0.0], false)).contains("nope"));
    assert!(invalid(&two_d("x", "nope", [0.0, 0.0], false)).contains("nope"));
    assert!(invalid(&two_d("x", "y", [0.0, 0.0], true)).contains("no samples"));
    assert!(invalid(&two_d("x", "y", [f32::INFINITY, 0.0], false)).contains("finite"));
    assert!(invalid(&two_d("x", "y", [0.0, f32::NAN], false)).contains("finite"));
}

#[test]
fn state_machine_references_and_numbers_are_checked() {
    assert!(
        graph(machine(vec![transition(Some("idle"), "run")]), &[])
            .validate()
            .is_ok()
    );
    assert!(invalid(&graph(machine(vec![transition(Some("ghost"), "run")]), &[])).contains("source"));
    assert!(invalid(&graph(machine(vec![transition(None, "ghost")]), &[])).contains("target"));
    for duration in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut t = transition(None, "run");
        t.duration = duration;
        assert!(
            invalid(&graph(machine(vec![t]), &[])).contains("finite"),
            "duration {duration}"
        );
    }
    let mut negative = transition(None, "run");
    negative.duration = -0.1;
    assert!(invalid(&graph(machine(vec![negative]), &[])).contains("negative"));
    let mut zero = transition(None, "run");
    zero.duration = 0.0;
    assert!(
        graph(machine(vec![zero]), &[]).validate().is_ok(),
        "an instant transition is fine"
    );
    // Initial state must exist, so a machine with no states is invalid.
    let none = graph(
        AnimNode::StateMachine(StateMachineDef {
            initial: "idle".into(),
            states: vec![],
            transitions: vec![],
        }),
        &[],
    );
    assert!(invalid(&none).contains("initial"));
}

#[test]
fn condition_parameters_must_be_declared_and_numbers_finite() {
    let with = |condition: Condition| {
        let mut t = transition(None, "run");
        t.conditions = vec![condition];
        graph(machine(vec![t]), &["Speed", "Grounded", "Count", "Jump"])
    };
    assert!(
        with(Condition::Float {
            param: "Speed".into(),
            op: CmpOp::Gt,
            value: 1.0
        })
        .validate()
        .is_ok()
    );
    assert!(
        with(Condition::Int {
            param: "Count".into(),
            op: CmpOp::Ge,
            value: 2
        })
        .validate()
        .is_ok()
    );
    assert!(
        with(Condition::Bool {
            param: "Grounded".into(),
            value: true
        })
        .validate()
        .is_ok()
    );
    assert!(
        with(Condition::Trigger { param: "Jump".into() })
            .validate()
            .is_ok()
    );
    assert!(with(Condition::ClipFinished).validate().is_ok());
    assert!(with(Condition::Notify("anything".into())).validate().is_ok());
    for condition in [
        Condition::Float {
            param: "nope".into(),
            op: CmpOp::Gt,
            value: 1.0,
        },
        Condition::Int {
            param: "nope".into(),
            op: CmpOp::Gt,
            value: 1,
        },
        Condition::Bool {
            param: "nope".into(),
            value: true,
        },
        Condition::Trigger { param: "nope".into() },
    ] {
        assert!(invalid(&with(condition)).contains("nope"));
    }
    // A NaN threshold would make the transition silently unreachable.
    for value in [f32::NAN, f32::INFINITY] {
        let message = invalid(&with(Condition::Float {
            param: "Speed".into(),
            op: CmpOp::Lt,
            value,
        }));
        assert!(message.contains("finite"), "{message}");
    }
}

fn ron_of(g: &AnimGraph) -> String {
    g.to_ron_string().expect("ron")
}

#[test]
fn loading_checks_the_version_before_the_body() {
    let g = graph(clip("idle"), &[]);
    let current = ron_of(&g);
    assert!(current.contains(&format!("format_version: {ANIMGRAPH_FORMAT_VERSION}")));
    assert_eq!(
        AnimGraph::from_ron_str(&current).expect("current"),
        g.triangulated()
    );
    // Round trip is stable.
    assert_eq!(ron_of(&AnimGraph::from_ron_str(&current).expect("load")), current);
    let at = |version: u32| {
        current.replace(
            &format!("format_version: {ANIMGRAPH_FORMAT_VERSION}"),
            &format!("format_version: {version}"),
        )
    };
    assert!(AnimGraph::from_ron_str(&at(1)).is_ok(), "v1 migrates");
    assert!(
        matches!(
            AnimGraph::from_ron_str(&at(0)),
            Err(AnimGraphError::UnknownVersion(0))
        ),
        "no step from 0"
    );
    assert!(matches!(
        AnimGraph::from_ron_str(&at(ANIMGRAPH_FORMAT_VERSION + 1)),
        Err(AnimGraphError::UnknownVersion(v)) if v == ANIMGRAPH_FORMAT_VERSION + 1
    ));
    assert!(matches!(
        AnimGraph::from_ron_str(&at(u32::MAX)),
        Err(AnimGraphError::UnknownVersion(u32::MAX))
    ));
    // Garbage, empty text and a missing version are errors, not panics.
    for text in [
        "",
        "(",
        "not ron at all",
        "(parameters: [], root: Clip((clip: \"x\", looping: true, speed: 1.0, sync_group: None)))",
    ] {
        assert!(AnimGraph::from_ron_str(text).is_err(), "{text:?}");
    }
    // A body that fails validation is refused with the reason.
    let broken = graph(clip("idle"), &[]);
    let text = ron_of(&broken).replace("speed: 1.0", "speed: NaN");
    assert!(matches!(
        AnimGraph::from_ron_str(&text),
        Err(AnimGraphError::Invalid(_)) | Err(AnimGraphError::RonParse(_))
    ));
    // Error messages name the versions involved.
    let message = AnimGraph::from_ron_str(&at(99)).expect_err("newer").to_string();
    assert!(
        message.contains("99") && message.contains(&ANIMGRAPH_FORMAT_VERSION.to_string()),
        "{message}"
    );
}

fn document(version: u32) -> AnimGraphDocument {
    AnimGraphDocument {
        format_version: version,
        parameters: vec![trigger_decl("T")],
        root: clip("c"),
    }
}

#[test]
fn migration_steps_chain_in_order_and_failures_name_their_step() {
    assert_eq!(
        registered_migrations()
            .iter()
            .map(|s| (s.from, s.to))
            .collect::<Vec<_>>(),
        // v2 -> v3 is the montage `Slot` node (spec 34), v3 -> v4 the IK nodes (ADR-8475); every
        // step stays in the chain.
        [(1, 2), (2, 3), (3, 4)]
    );
    let mut doc = document(1);
    assert_eq!(migrate_document(&mut doc).expect("migrate"), 3);
    assert_eq!(doc.format_version, ANIMGRAPH_FORMAT_VERSION);
    assert_eq!(
        migrate_document(&mut doc).expect("again"),
        0,
        "already current: nothing to do"
    );
    assert!(matches!(
        migrate_document(&mut document(0)),
        Err(AnimGraphError::UnknownVersion(0))
    ));
    assert!(matches!(
        migrate_document(&mut document(ANIMGRAPH_FORMAT_VERSION + 1)),
        Err(AnimGraphError::UnknownVersion(_))
    ));

    // A failing step stops the chain, leaves the version where it was, and names itself.
    fn fail(_: &mut AnimGraphDocument) -> Result<(), String> {
        Err("boom".into())
    }
    let steps = [MigrationStep {
        from: 1,
        to: 2,
        apply: fail,
    }];
    let mut doc = document(1);
    let error = migrate_with(&mut doc, &steps).expect_err("fails").to_string();
    assert!(
        error.contains("1") && error.contains("2") && error.contains("boom"),
        "{error}"
    );
    assert_eq!(doc.format_version, 1);
    // A table with a gap (or a step that skips a version) does not reach the target.
    let skip = [MigrationStep {
        from: 1,
        to: 3,
        apply: |_| Ok(()),
    }];
    assert!(matches!(
        migrate_with(&mut document(1), &skip),
        Err(AnimGraphError::UnknownVersion(1))
    ));
    assert!(matches!(
        migrate_with(&mut document(1), &[]),
        Err(AnimGraphError::UnknownVersion(1))
    ));
    // Steps can modify the document.
    let tag = [
        MigrationStep {
            from: 1,
            to: 2,
            apply: |d| {
                d.parameters.clear();
                Ok(())
            },
        },
        MigrationStep {
            from: 2,
            to: 3,
            apply: |_| Ok(()),
        },
        MigrationStep {
            from: 3,
            to: 4,
            apply: |_| Ok(()),
        },
    ];
    let mut doc = document(1);
    assert_eq!(migrate_with(&mut doc, &tag).expect("ok"), 3);
    assert!(doc.parameters.is_empty());
}
