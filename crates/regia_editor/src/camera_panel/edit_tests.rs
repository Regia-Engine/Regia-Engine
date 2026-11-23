// SPDX-License-Identifier: MPL-2.0
//! The rig editor's forms, structure edits, history, New and autosave (ADR-7734).

use regia_framework::camera::asset::{Condition, DirectorDef, RigMatch, Value as NodeValue};

use super::edit::NODE_KINDS;
use super::*;
use crate::gameplay_panel::reason_of;

const GOOD: &str = r#"(format_version: 2, name: "player",
    params: { "len": Float(4.0) },
    variables: [(name: "aiming", default: Bool(false))],
    inputs: [(name: "look", kind: Axis2, speed: (120.0, 90.0), pitch_clamp: (-80.0, 30.0))],
    rigs: [
        (name: "walk", root: [
            Boom(length: Param("len")),
            Group(enabled: Literal(true), nodes: [Fov(degrees: Literal(70.0))]),
        ], transitions: [(from: Rig("walk"), to: Rig("aim"), blend: Smooth(0.25))]),
        (name: "aim", root: [Fov(degrees: Literal(40.0))]),
        (name: "spare", root: [Fov(degrees: Literal(50.0))]),
    ],
    transitions: [(from: Any, to: Any, blend: Linear(0.5))],
    director: Rules(default: "walk", rules: [(when: VarBool("aiming", true), rig: "aim")]))"#;

fn project(tag: &str, rel: &str, text: &str) -> (World, PathBuf) {
    let dir = regia_test_utils::unique_temp_dir(tag);
    let file = dir.join("assets").join(rel);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
    regia_test_utils::write_atomic(&file, text.as_bytes()).expect("write");
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    panel_load(&mut world, &json!({ "path": rel })).expect("load");
    (world, dir)
}

fn op(world: &mut World, params: Value) -> Result<Value, RpcError> {
    panel_edit(world, &params)
}

fn ok(world: &mut World, params: Value) -> Value {
    op(world, params.clone()).unwrap_or_else(|e| panic!("{params}: {e:?}"))
}

fn code(result: Result<Value, RpcError>) -> String {
    reason_of(&result.expect_err("refused")).unwrap_or_default()
}

fn working(world: &World) -> CameraAsset {
    world
        .resource::<CameraAssets>()
        .working("player")
        .cloned()
        .expect("working copy")
}

fn bytes(world: &World) -> String {
    working(world).to_ron().expect("canonical")
}

/// The real runtime loader takes `asset` (parsed, migrated and validated) without errors.
fn loads(asset: &CameraAsset) {
    let text = asset.to_ron().expect("canonical");
    assert_eq!(
        &CameraAsset::from_ron(&text).expect("parses"),
        asset,
        "round trip"
    );
    let mut play = World::new();
    let mut hooks = CameraHooks::default();
    hooks.accept_any_name();
    play.insert_resource(hooks);
    let status =
        load::load_camera_text(&mut play, "x.camera.ron", &text, false).expect("the loader reads it");
    assert_ne!(
        status,
        CameraBuildStatus::Errors,
        "{:?}",
        play.resource::<CameraAssets>().issues(&asset.name)
    );
}

// ------------------------------------------------------------------------- (1) node forms

/// One rig `all` holding one default node of every kind, in [`NODE_KINDS`] order.
fn all_kinds_text() -> String {
    let mut asset = CameraAsset::from_ron(GOOD).expect("parses");
    let mut root = Vec::new();
    for kind in NODE_KINDS {
        root.push(edit::new_node(&asset, kind).expect("node"));
    }
    asset.rigs.push(regia_framework::camera::asset::RigDef {
        name: "all".into(),
        tags: Vec::new(),
        root,
        transitions: Vec::new(),
        blend_out: None,
    });
    asset.to_ron().expect("text")
}

#[test]
fn every_node_kind_edits_one_field_through_the_form_and_saves_the_canonical_text() {
    // (kind, field, value): the form value set, and the change it must make.
    let table: [(&str, &str, Value); 16] = [
        ("Pivot", "offset[1]", json!(2.5)),
        ("LookInput", "additive", json!(true)),
        ("Boom", "length.Literal", json!(6.0)),
        ("Collision", "radius", json!(0.5)),
        ("Fov", "degrees.Literal", json!(55.0)),
        ("Offset", "translation[0]", json!(1.0)),
        ("SmoothPosition", "depth", json!(8.0)),
        ("SmoothRotation", "yaw", json!(3.0)),
        ("Lean", "gain", json!(0.3)),
        ("Frame", "speed", json!(2.0)),
        ("Shake", "amplitude", json!(0.2)),
        ("Fly", "speed", json!(7.0)),
        ("Rail", "spline", json!("track")),
        ("Crane", "length.Literal", json!(3.0)),
        ("Group", "enabled.Literal", json!(false)),
        ("When", "cond.Elapsed", json!(1.5)),
    ];
    let text = all_kinds_text();
    for (index, (kind, field, value)) in table.iter().enumerate() {
        assert_eq!(NODE_KINDS[index], *kind, "the table follows NODE_KINDS");
        let (mut world, dir) = project("camera-form-kind", "p.camera.ron", &text);
        let mut expected = working(&world);
        {
            let node = &mut expected.rigs[3].root[index];
            match node {
                Node::Pivot { offset, .. } => offset[1] = 2.5,
                Node::LookInput { additive, .. } => *additive = true,
                Node::Boom { length, .. } | Node::Crane { length, .. } => {
                    *length = NodeValue::Literal(if *kind == "Boom" { 6.0 } else { 3.0 });
                }
                Node::Collision { radius, .. } => *radius = 0.5,
                Node::Fov { degrees } => *degrees = NodeValue::Literal(55.0),
                Node::Offset { translation, .. } => translation[0] = 1.0,
                Node::SmoothPosition { depth, .. } => *depth = 8.0,
                Node::SmoothRotation { yaw, .. } => *yaw = 3.0,
                Node::Lean { gain, .. } => *gain = 0.3,
                Node::Frame { speed, .. } => *speed = 2.0,
                Node::Shake { amplitude, .. } => *amplitude = 0.2,
                Node::Fly { speed, .. } => *speed = 7.0,
                Node::Rail { spline, .. } => "track".clone_into(spline),
                Node::Group { enabled, .. } => *enabled = NodeValue::Literal(false),
                Node::When { cond, .. } => *cond = Condition::Elapsed(1.5),
            }
        }
        let reply = ok(
            &mut world,
            json!({ "op": "node_field", "rig": "all", "node": [index], "field": field, "value": value }),
        );
        assert_eq!(reply["status"], "stale", "{kind}");
        assert_eq!(reply["history"]["undo_depth"], 1, "{kind}: one step");
        assert_eq!(reply["history"]["dirty"], true, "{kind}");
        panel_save(&mut world, &json!({})).unwrap_or_else(|e| panic!("{kind}: {e:?}"));
        let saved = std::fs::read_to_string(dir.join("assets/p.camera.ron")).expect("read");
        assert_eq!(saved, expected.to_ron().expect("expected text"), "{kind} {field}");
        loads(&expected);
        assert_eq!(get_history(&mut world)["dirty"], false, "{kind}: saved");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn get_history(world: &mut World) -> Value {
    panel_get(world, &json!({})).expect("get")["history"].clone()
}

#[test]
fn a_field_switches_variant_and_bad_field_edits_are_refused_with_codes() {
    let (mut world, dir) = project("camera-form-variant", "p.camera.ron", GOOD);
    // Boom's length is Param("len"): make it a literal, then a variable.
    ok(
        &mut world,
        json!({ "op": "node_variant", "rig": "walk", "node": [0], "field": "length", "variant": "Literal" }),
    );
    assert!(matches!(
        working(&world).rigs[0].root[0],
        Node::Boom {
            length: NodeValue::Literal(_),
            ..
        }
    ));
    ok(
        &mut world,
        json!({ "op": "node_variant", "rig": "walk", "node": [0], "field": "length", "variant": "Var" }),
    );
    ok(
        &mut world,
        json!({ "op": "node_field", "rig": "walk", "node": [0], "field": "length.Var", "value": "aiming" }),
    );
    assert!(matches!(
        &working(&world).rigs[0].root[0],
        Node::Boom { length: NodeValue::Var(name), .. } if name == "aiming"
    ));
    // A transition's blend and a rig's tags through the same forms.
    ok(
        &mut world,
        json!({ "op": "transition_variant", "index": 0, "field": "blend", "variant": "Cut" }),
    );
    ok(
        &mut world,
        json!({ "op": "transition_field", "owner": "walk", "index": 0, "field": "blend.Smooth", "value": 0.75 }),
    );
    ok(
        &mut world,
        json!({ "op": "rig_field", "rig": "aim", "field": "tags", "value": ["aim"] }),
    );
    let asset = working(&world);
    assert_eq!(
        asset.transitions[0].blend,
        regia_framework::camera::asset::Blend::Cut
    );
    assert_eq!(
        asset.rigs[0].transitions[0].blend,
        regia_framework::camera::asset::Blend::Smooth(0.75)
    );
    assert_eq!(asset.rigs[1].tags, ["aim"]);
    for (params, expected) in [
        (
            json!({ "op": "node_field", "rig": "walk", "node": [1], "field": "nodes", "value": [] }),
            "camera_panel_structure_field",
        ),
        (
            json!({ "op": "node_field", "rig": "walk", "node": [0], "field": "nope", "value": 1 }),
            "camera_panel_no_field",
        ),
        (
            json!({ "op": "node_field", "rig": "walk", "node": [0], "field": "length.Var", "value": 3 }),
            "camera_bad_value",
        ),
        (
            json!({ "op": "node_variant", "rig": "walk", "node": [0], "field": "length", "variant": "Nope" }),
            "camera_bad_value",
        ),
        (
            json!({ "op": "node_field", "rig": "walk", "node": [7], "field": "x", "value": 1 }),
            "camera_panel_unknown_node",
        ),
        (
            json!({ "op": "node_field", "rig": "nope", "node": [0], "field": "x", "value": 1 }),
            "camera_unknown_rig",
        ),
        (
            json!({ "op": "rig_field", "rig": "aim", "field": "name", "value": "x" }),
            "camera_panel_no_field",
        ),
        (
            json!({ "op": "transition_field", "index": 9, "field": "blend", "value": "Cut" }),
            "camera_panel_unknown_transition",
        ),
        (json!({ "op": "nope" }), "bad_op"),
    ] {
        let before = bytes(&world);
        assert_eq!(code(op(&mut world, params.clone())), expected, "{params}");
        assert_eq!(bytes(&world), before, "a refused edit changes nothing: {params}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_with_comments_warns_before_the_edit_that_drops_them() {
    let text = format!("// the player's camera\n{GOOD}");
    let (mut world, dir) = project("camera-form-comments", "p.camera.ron", &text);
    assert_eq!(
        get_history(&mut world)["comments_warning"],
        history::COMMENTS_WARNING
    );
    let reply = ok(&mut world, json!({ "param": "len", "value": 5.0 }));
    assert_eq!(reply["comments_dropped"], true);
    panel_save(&mut world, &json!({})).expect("save");
    let saved = std::fs::read_to_string(dir.join("assets/p.camera.ron")).expect("read");
    assert!(!crate::abilities_panel::form::has_comments(&saved));
    assert_eq!(get_history(&mut world)["comments_warning"], Value::Null);
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------------------------------- (2) structure edits

#[test]
fn structure_edits_keep_the_asset_loadable_and_references_valid() {
    // (params, what the working copy must show afterwards)
    type Check = fn(&CameraAsset) -> bool;
    let table: Vec<(Value, Check)> = vec![
        (json!({ "op": "add_node", "rig": "aim", "kind": "Boom" }), |a| {
            matches!(a.rigs[1].root[1], Node::Boom { .. })
        }),
        (
            json!({ "op": "add_node", "rig": "walk", "parent": [1], "index": 0, "kind": "Shake" }),
            |a| matches!(a.rigs[0].root[1].children()[0], Node::Shake { .. }),
        ),
        (
            json!({ "op": "remove_node", "rig": "walk", "node": [1, 0] }),
            |a| a.rigs[0].root[1].children().is_empty(),
        ),
        (
            json!({ "op": "duplicate_node", "rig": "walk", "node": [0] }),
            |a| a.rigs[0].root.len() == 3 && a.rigs[0].root[0] == a.rigs[0].root[1],
        ),
        (
            json!({ "op": "move_node", "rig": "walk", "node": [0], "to": 1 }),
            |a| matches!(a.rigs[0].root[1], Node::Boom { .. }),
        ),
        (json!({ "op": "add_rig" }), |a| a.rigs[3].name == "rig"),
        (json!({ "op": "add_rig", "name": "crouch" }), |a| {
            a.rigs[3].name == "crouch"
        }),
        (json!({ "op": "remove_rig", "rig": "spare" }), |a| {
            a.rigs.len() == 2
        }),
        (json!({ "op": "duplicate_rig", "rig": "walk" }), |a| {
            a.rigs[1].name == "walk_copy" && a.rigs[1].root == a.rigs[0].root
        }),
        (json!({ "op": "move_rig", "rig": "spare", "to": 0 }), |a| {
            a.rigs[0].name == "spare"
        }),
        (json!({ "op": "rename_rig", "rig": "aim", "name": "zoom" }), |a| {
            a.rigs[1].name == "zoom"
                && a.rigs[0].transitions[0].to == RigMatch::Rig("zoom".into())
                && matches!(&a.director, DirectorDef::Rules { rules, .. } if rules[0].rig == "zoom")
        }),
        (
            json!({ "op": "rename_rig", "rig": "walk", "name": "stroll" }),
            |a| {
                a.rigs[0].transitions[0].from == RigMatch::Rig("stroll".into())
                    && matches!(&a.director, DirectorDef::Rules { default, .. } if default == "stroll")
            },
        ),
        (
            json!({ "op": "add_transition", "from_rig": "spare", "to_rig": "tag:aim" }),
            |a| {
                a.transitions[1].from == RigMatch::Rig("spare".into())
                    && a.transitions[1].to == RigMatch::Tag("aim".into())
            },
        ),
        (json!({ "op": "add_transition", "owner": "aim" }), |a| {
            a.rigs[1].transitions.len() == 1
        }),
        (json!({ "op": "remove_transition", "index": 0 }), |a| {
            a.transitions.is_empty()
        }),
        (
            json!({ "op": "duplicate_transition", "owner": "walk", "index": 0 }),
            |a| a.rigs[0].transitions.len() == 2 && a.rigs[0].transitions[0] == a.rigs[0].transitions[1],
        ),
        (
            json!({ "op": "move_transition", "owner": "walk", "index": 0, "to": 0 }),
            |a| a.rigs[0].transitions.len() == 1,
        ),
        (
            json!({ "op": "set_group", "rig": "walk", "node": [1], "enabled": false }),
            |a| {
                matches!(
                    a.rigs[0].root[1],
                    Node::Group {
                        enabled: NodeValue::Literal(false),
                        ..
                    }
                )
            },
        ),
    ];
    for (params, check) in table {
        let (mut world, dir) = project("camera-structure", "p.camera.ron", GOOD);
        ok(&mut world, params.clone());
        let asset = working(&world);
        assert!(check(&asset), "{params}: {}", asset.to_ron().unwrap_or_default());
        loads(&asset);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn structure_edits_that_would_break_the_asset_are_refused_with_codes() {
    let (mut world, dir) = project("camera-structure-refuse", "p.camera.ron", GOOD);
    for (params, expected) in [
        // `aim` is named by the director's rule and by walk's transition.
        (
            json!({ "op": "remove_rig", "rig": "aim" }),
            "camera_panel_rig_in_use",
        ),
        (
            json!({ "op": "remove_rig", "rig": "walk" }),
            "camera_panel_rig_in_use",
        ),
        (json!({ "op": "remove_rig", "rig": "nope" }), "camera_unknown_rig"),
        (
            json!({ "op": "rename_rig", "rig": "aim", "name": "walk" }),
            "camera_panel_name_taken",
        ),
        (
            json!({ "op": "rename_rig", "rig": "aim", "name": " " }),
            "camera_bad_value",
        ),
        (
            json!({ "op": "add_rig", "name": "aim" }),
            "camera_panel_name_taken",
        ),
        (
            json!({ "op": "duplicate_rig", "rig": "aim", "name": "spare" }),
            "camera_panel_name_taken",
        ),
        (
            json!({ "op": "move_rig", "rig": "aim", "to": 3 }),
            "camera_panel_bad_index",
        ),
        (
            json!({ "op": "add_node", "rig": "walk", "kind": "Nope" }),
            "camera_panel_bad_kind",
        ),
        (
            json!({ "op": "add_node", "rig": "walk", "parent": [0], "kind": "Fov" }),
            "camera_panel_not_a_group",
        ),
        (
            json!({ "op": "add_node", "rig": "walk", "index": 9, "kind": "Fov" }),
            "camera_panel_bad_index",
        ),
        (
            json!({ "op": "remove_node", "rig": "walk", "node": [5] }),
            "camera_panel_unknown_node",
        ),
        (
            json!({ "op": "remove_node", "rig": "walk", "node": [] }),
            "camera_panel_unknown_node",
        ),
        (
            json!({ "op": "move_node", "rig": "walk", "node": [0], "to": 2 }),
            "camera_panel_bad_index",
        ),
        (
            json!({ "op": "add_transition", "to_rig": "nope" }),
            "camera_unknown_rig",
        ),
        (
            json!({ "op": "add_transition", "owner": "nope" }),
            "camera_unknown_rig",
        ),
        (
            json!({ "op": "remove_transition", "index": 4 }),
            "camera_panel_unknown_transition",
        ),
        (
            json!({ "op": "move_transition", "index": 0, "to": 1 }),
            "camera_panel_bad_index",
        ),
        (
            json!({ "op": "set_group", "rig": "walk", "node": [0], "enabled": true }),
            "camera_panel_unknown_node",
        ),
    ] {
        let before = bytes(&world);
        assert_eq!(code(op(&mut world, params.clone())), expected, "{params}");
        assert_eq!(bytes(&world), before, "{params}");
    }
    assert_eq!(get_history(&mut world)["undo_depth"], 0, "a refusal is no step");
    // Removing what names the rig first lets it go.
    ok(
        &mut world,
        json!({ "op": "remove_transition", "owner": "walk", "index": 0 }),
    );
    assert_eq!(
        code(op(&mut world, json!({ "op": "remove_rig", "rig": "aim" }))),
        "camera_panel_rig_in_use",
        "the director's rule still names it"
    );
    world.resource_mut::<CameraAssets>().edit("player", |asset| {
        asset.director = DirectorDef::Fixed("walk".into());
    });
    ok(&mut world, json!({ "op": "remove_rig", "rig": "aim" }));
    loads(&working(&world));
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------------------ (3) undo

/// A small deterministic generator (xorshift64*), so the property test needs no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// A random edit op for the current working copy (it may be refused; refusals are no step).
fn random_op(rng: &mut Rng, asset: &CameraAsset) -> Value {
    let rig = &asset.rigs[rng.below(asset.rigs.len())];
    let node = rng.below(rig.root.len().max(1));
    let kind = NODE_KINDS[rng.below(NODE_KINDS.len())];
    match rng.below(14) {
        0 => json!({ "op": "add_node", "rig": rig.name, "kind": kind }),
        1 => json!({ "op": "remove_node", "rig": rig.name, "node": [node] }),
        2 => json!({ "op": "duplicate_node", "rig": rig.name, "node": [node] }),
        3 => {
            json!({ "op": "move_node", "rig": rig.name, "node": [node], "to": rng.below(rig.root.len().max(1)) })
        }
        4 => json!({ "op": "add_rig" }),
        5 => json!({ "op": "duplicate_rig", "rig": rig.name }),
        6 => json!({ "op": "move_rig", "rig": rig.name, "to": rng.below(asset.rigs.len()) }),
        7 => json!({ "op": "rename_rig", "rig": rig.name, "name": format!("r{}", rng.below(1000)) }),
        8 => json!({ "op": "remove_rig", "rig": rig.name }),
        9 => json!({ "op": "set_param", "param": "len", "value": rng.below(100) as f64 / 4.0 }),
        10 => {
            json!({ "op": "add_transition", "owner": if rng.below(2) == 0 { json!(null) } else { json!(rig.name) } })
        }
        11 => {
            let variant = ["Cut", "Linear", "Smooth", "EaseIn"][rng.below(4)];
            json!({ "op": "transition_variant", "index": 0, "field": "blend", "variant": variant })
        }
        12 => {
            json!({ "op": "rig_field", "rig": rig.name, "field": "blend_out", "value": rng.below(8) as f64 / 2.0 })
        }
        _ => match rig.root.get(node) {
            Some(Node::Fov { .. }) => {
                json!({ "op": "node_field", "rig": rig.name, "node": [node], "field": "degrees.Literal", "value": 30 + rng.below(60) })
            }
            Some(Node::Boom { .. }) => {
                json!({ "op": "node_variant", "rig": rig.name, "node": [node], "field": "length", "variant": "Literal" })
            }
            _ => json!({ "op": "set_group", "rig": "walk", "node": [1], "enabled": rng.below(2) == 0 }),
        },
    }
}

#[test]
fn fifty_random_edits_then_full_undo_return_the_original_bytes() {
    for seed in [1u64, 7, 42, 1234, 99_991, 0x5eed_cafe] {
        let (mut world, dir) = project("camera-undo-property", "p.camera.ron", GOOD);
        let original = bytes(&world);
        let mut rng = Rng(seed);
        let mut steps = 0;
        let mut tries = 0;
        while steps < 50 {
            tries += 1;
            assert!(tries < 5000, "seed {seed}: too many refusals");
            let params = random_op(&mut rng, &working(&world));
            let before = bytes(&world);
            if op(&mut world, params).is_ok() && bytes(&world) != before {
                steps += 1;
            }
        }
        let edited = bytes(&world);
        let history = get_history(&mut world);
        assert_eq!(history["undo_depth"], 50, "seed {seed}: one step per edit");
        assert_eq!(history["dirty"], true);
        for _ in 0..50 {
            ok(&mut world, json!({ "op": "undo" }));
        }
        assert_eq!(bytes(&world), original, "seed {seed}: full undo");
        assert_eq!(
            get_history(&mut world)["dirty"],
            false,
            "seed {seed}: back at the saved state"
        );
        assert_eq!(
            code(op(&mut world, json!({ "op": "undo" }))),
            "camera_panel_nothing_to_undo"
        );
        for _ in 0..50 {
            ok(&mut world, json!({ "op": "redo" }));
        }
        assert_eq!(bytes(&world), edited, "seed {seed}: full redo");
        assert_eq!(
            code(op(&mut world, json!({ "op": "redo" }))),
            "camera_panel_nothing_to_redo"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn gui_drags_merge_into_one_step_until_sealed_and_a_reload_starts_a_new_history() {
    let (mut world, dir) = project("camera-undo-merge", "p.camera.ron", GOOD);
    with_panel(&mut world, |panel, world| {
        for value in [5.0, 6.0, 7.0] {
            let key = Some("param/len".to_owned());
            run_edit_op(
                panel,
                world,
                "player",
                "set_param",
                &json!({ "param": "len", "value": value }),
                key,
            )
            .expect("drag");
        }
    });
    assert_eq!(get_history(&mut world)["undo_depth"], 1, "one drag, one step");
    with_panel(&mut world, |panel, world| {
        if let Some(doc) = sync_doc(panel, world, "player") {
            doc.undo.seal();
        }
        run_edit_op(
            panel,
            world,
            "player",
            "set_param",
            &json!({ "param": "len", "value": 8.0 }),
            Some("param/len".to_owned()),
        )
        .expect("second drag");
    });
    assert_eq!(get_history(&mut world)["undo_depth"], 2);
    ok(&mut world, json!({ "op": "undo" }));
    assert_eq!(working(&world).params["len"].as_f32(), Some(7.0));
    // The file changes on disk and reloads: the history starts again from it.
    regia_test_utils::write_atomic(
        &dir.join("assets/p.camera.ron"),
        GOOD.replace("Float(4.0)", "Float(2.0)").as_bytes(),
    )
    .expect("write");
    panel_load(&mut world, &json!({ "path": "p.camera.ron" })).expect("reload");
    let history = get_history(&mut world);
    assert_eq!(
        (history["undo_depth"].clone(), history["dirty"].clone()),
        (json!(0), json!(false))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------- (4) New and autosave

#[test]
fn content_new_makes_a_camera_rig_that_loads_valid_and_edits() {
    let dir = regia_test_utils::unique_temp_dir("camera-new");
    let kind = crate::content::NewAssetKind::parse("camera").expect("kind");
    assert!(
        crate::content::NewAssetKind::ALL.contains(&kind),
        "in the New menu"
    );
    assert_eq!(kind.label(), "Camera rig");
    let made = crate::content::create_asset(&dir, kind, "assets/cams/hero.camera.ron").expect("create");
    assert_eq!(made, "assets/cams/hero.camera.ron");
    let text = std::fs::read_to_string(dir.join(&made)).expect("read");
    let asset = CameraAsset::from_ron(&text).expect("parses");
    assert_eq!(asset.name, "hero");
    assert!(asset.validate(&|_| false).is_empty(), "no issues at all");
    assert!(
        crate::content::create_asset(&dir, kind, "assets/cams/hero.camera.ron").is_err(),
        "exists"
    );
    assert!(
        crate::content::create_asset(&dir, kind, "assets/cams/x.ron").is_err(),
        "extension"
    );
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let loaded = panel_load(&mut world, &json!({ "path": "cams/hero.camera.ron" })).expect("load");
    assert_eq!(loaded["status"], "ok");
    ok(&mut world, json!({ "op": "add_rig", "name": "aim" }));
    ok(&mut world, json!({ "op": "add_transition", "to_rig": "aim" }));
    let saved = panel_save(&mut world, &json!({})).expect("save");
    assert_eq!(saved["status"], "ok");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unsaved_edit_is_recovered_from_its_autosave() {
    let (mut world, dir) = project("camera-autosave", "cams/p.camera.ron", GOOD);
    assert!(
        (history::PROVIDER.dirty)(&world).is_empty(),
        "nothing unsaved yet"
    );
    ok(&mut world, json!({ "op": "add_rig", "name": "crouch" }));
    let edited = bytes(&world);
    let keys = (history::PROVIDER.dirty)(&world);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].kind, "camera");
    assert_eq!(
        keys[0].original.as_deref(),
        Some(Path::new("assets/cams/p.camera.ron"))
    );
    let snapshots = (history::PROVIDER.snapshot)(&world);
    assert_eq!(
        String::from_utf8(snapshots[0].bytes.clone()).expect("text"),
        edited
    );
    // The editor goes down; the autosave is written and recovered in a fresh session.
    crate::doc_autosave::write_docs(&dir, &snapshots).expect("autosave");
    drop(world);
    let candidates = crate::doc_autosave::doc_recovery_candidates(&dir);
    assert_eq!(candidates.len(), 1);
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let bytes_on_disk = std::fs::read(&candidates[0].autosave).expect("autosave bytes");
    (history::PROVIDER.restore)(&mut world, &candidates[0].key(), &bytes_on_disk).expect("restore");
    assert_eq!(bytes(&world), edited, "the unsaved edit is back");
    let v = panel_get(&mut world, &json!({})).expect("get");
    assert_eq!(v["selected_asset"], "player");
    assert_eq!(v["history"]["dirty"], true, "recovered work is unsaved");
    assert_eq!(v["page"]["row"]["path"], "cams/p.camera.ron", "at its own file");
    let file = std::fs::read_to_string(dir.join("assets/cams/p.camera.ron")).expect("read");
    assert_eq!(file, GOOD, "the file is untouched until a save");
    panel_save(&mut world, &json!({})).expect("save");
    assert_eq!(
        std::fs::read_to_string(dir.join("assets/cams/p.camera.ron")).expect("read"),
        edited
    );
    assert!((history::PROVIDER.dirty)(&world).is_empty(), "saved");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_camera_kind_is_registered_for_autosave() {
    let mut registry = crate::doc_autosave::DocAutosaveRegistry::default();
    crate::doc_autosave_providers::register_builtin(&mut registry);
    assert!(registry.kinds().contains(&"camera"));
}

// ---------------------------------------------------------------------- (5) GUI and agent

fn draw_frame(world: &mut World) -> usize {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 1400.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| camera_panel(ui, world));
    output.textures_delta.clear();
    ctx.tessellate(output.shapes, output.pixels_per_point).len()
}

#[test]
fn the_rig_editor_draws_every_node_form_and_the_structure_controls() {
    let (mut world, dir) = project("camera-draw-forms", "p.camera.ron", &all_kinds_text());
    world.resource_mut::<CameraPanel>().tab = CameraTab::Asset;
    assert!(draw_frame(&mut world) > 0);
    ok(&mut world, json!({ "param": "len", "value": 3.0 }));
    assert!(draw_frame(&mut world) > 0, "with a history");
    for (index, kind) in NODE_KINDS.iter().enumerate() {
        {
            let mut panel = world.resource_mut::<CameraPanel>();
            panel.tab = CameraTab::Graph;
            panel.selected_node = Some((3, vec![index]));
        }
        assert!(draw_frame(&mut world) > 0, "{kind}");
        assert_eq!(
            world.resource::<CameraPanel>().selected_node,
            Some((3, vec![index])),
            "{kind}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_graph_and_page_carry_the_forms_the_edit_ops_take() {
    let (mut world, dir) = project("camera-agent-forms", "p.camera.ron", GOOD);
    let g = panel_graph(&mut world, &json!({})).expect("graph");
    let boom = &g["rigs"][0]["nodes"][0]["form"];
    assert_eq!(boom["kind"], "Boom");
    assert_eq!(boom["fields"]["length"], json!({ "Param": "len" }));
    assert!(
        boom["variants"]["length"]
            .as_array()
            .is_some_and(|v| v.contains(&json!("Literal")))
    );
    assert_eq!(
        g["rigs"][0]["nodes"][1]["form"]["fields"]["nodes"], 1,
        "a group shows a count"
    );
    let v = panel_get(&mut world, &json!({})).expect("get");
    let rigs = v["page"]["rigs"].as_array().expect("rigs");
    assert_eq!(rigs[1]["name"], "aim");
    assert_eq!(rigs[1]["references"].as_array().map(Vec::len), Some(2));
    assert_eq!(rigs[2]["references"], json!([]));
    let transitions = v["page"]["transitions"].as_array().expect("transitions");
    assert_eq!(transitions.len(), 2);
    assert_eq!(transitions[0]["owner"], Value::Null);
    assert_eq!(transitions[1]["owner"], "walk");
    assert_eq!(
        transitions[1]["form"]["fields"]["blend"],
        json!({ "Smooth": 0.25 })
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_edit_schema_lists_every_op() {
    let list = methods();
    let edit = list
        .iter()
        .find(|(schema, _)| schema.name == "editor.camera_panel_edit")
        .expect("edit");
    let op_doc = edit.0.params.iter().find(|p| p.name == "op").expect("op param");
    for name in EDIT_OPS {
        assert!(op_doc.description.contains(name), "{name}");
    }
    let names: std::collections::BTreeSet<_> = edit.0.params.iter().map(|p| p.name).collect();
    assert_eq!(names.len(), edit.0.params.len(), "no parameter twice");
}
