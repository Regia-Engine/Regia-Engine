// SPDX-License-Identifier: MPL-2.0
//! Definition editor tests: every section's op saves the canonical bytes of the expected definition
//! and the runtime loader reads them back; a long random edit sequence undoes to the original bytes;
//! the validator's message gates Save; comments are warned about; the agent handlers; headless drawing.

use std::path::PathBuf;

use regia_ai::smart_object_def::SmartObjectDefError;
use serde_json::json;

use super::*;
use crate::data_panels::test_ui::draw_frames;

const REL: &str = "assets/props/bench.smartobject.ron";

fn handler(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered")
}

/// A bench: two seats, the left one with an entrance and its own behavior.
fn bench() -> SmartObjectDef {
    let mut def = SmartObjectDef::new();
    def.activity = vec!["Activity.Sit".into()];
    def.behaviors = vec![BehaviorDef {
        family: "sit".into(),
        data: "(pose: \"upright\")".into(),
    }];
    let mut left = default_slot("left");
    left.offset.translation = (-0.4, 0.0, 0.0);
    left.entrances.push(EntranceDef {
        offset: OffsetDef {
            translation: (0.0, 0.0, 1.0),
            ..OffsetDef::default()
        },
        kind: EntranceKindDef::Enter,
        rank: 2,
    });
    let mut right = default_slot("right");
    right.offset.translation = (0.4, 0.0, 0.0);
    def.slots = vec![left, right];
    def
}

/// A world with a project holding `text` (the canonical bench when `None`), opened in the editor.
fn fixture(text: Option<&str>) -> (World, PathBuf, String) {
    let root = regia_test_utils::unique_temp_dir("smart-object-def-panel");
    let text = text.map_or_else(|| bench().to_ron().expect("serialize"), str::to_owned);
    let file = root.join(REL);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, &text).expect("write");
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject { root: Some(root) });
    world.insert_resource(UndoStack::new());
    open_def(&mut world, REL).expect("open");
    (world, file, text)
}

fn def_of(world: &World) -> SmartObjectDef {
    world
        .resource::<SmartObjectDefPanel>()
        .doc()
        .expect("open")
        .def
        .clone()
}

/// One op per section of the form, with the definition it must produce from the bench.
fn section_cases() -> Vec<(&'static str, SmartObjectDefOp, SmartObjectDef)> {
    let query: TagQuery =
        ron::from_str("AnyOf([Has(\"Role.Guard\"), Has(\"Role.Civilian\")])").expect("query");
    let condition = ConditionDef {
        path: "my_game::BenchIntact".into(),
        params: BTreeMap::from([("min".to_owned(), "2".to_owned())]),
    };
    let behavior = BehaviorDef {
        family: "guard_sit".into(),
        data: "(pose: \"alert\")".into(),
    };
    let entrance = EntranceDef {
        offset: OffsetDef {
            translation: (0.5, 0.0, 1.5),
            rotation: (0.0, 0.382_683_43, 0.0, 0.923_879_5),
        },
        kind: EntranceKindDef::Exit,
        rank: -1,
    };
    let offset = OffsetDef {
        translation: (-0.5, 0.1, 0.25),
        rotation: (0.0, 0.707_106_77, 0.0, 0.707_106_77),
    };
    let with = |f: &dyn Fn(&mut SmartObjectDef)| {
        let mut def = bench();
        f(&mut def);
        def
    };
    vec![
        (
            "activity (definition)",
            SmartObjectDefOp::Activity {
                scope: DefScope::Definition,
                tags: vec!["Activity.Sit".into(), "Activity.Rest".into()],
            },
            with(&|d| d.activity.push("Activity.Rest".into())),
        ),
        (
            "activity (slot)",
            SmartObjectDefOp::Activity {
                scope: DefScope::Slot(1),
                tags: vec!["Activity.Lie".into()],
            },
            with(&|d| d.slots[1].activity = vec!["Activity.Lie".into()]),
        ),
        (
            "rules: activity merge",
            SmartObjectDefOp::ActivityMerge(ActivityMergeDef::SlotFirst),
            with(&|d| d.activity_merge = ActivityMergeDef::SlotFirst),
        ),
        (
            "rules: requester rule",
            SmartObjectDefOp::UserRule(UserRuleDef::SlotElseDefinition),
            with(&|d| d.user_rule = UserRuleDef::SlotElseDefinition),
        ),
        (
            "queries (definition)",
            SmartObjectDefOp::UserQuery {
                scope: DefScope::Definition,
                query: Some(query.clone()),
            },
            with(&|d| d.user_query = Some(query.clone())),
        ),
        (
            "queries (slot)",
            SmartObjectDefOp::UserQuery {
                scope: DefScope::Slot(0),
                query: Some(query.clone()),
            },
            with(&|d| d.slots[0].user_query = Some(query.clone())),
        ),
        (
            "conditions (definition)",
            SmartObjectDefOp::Conditions {
                scope: DefScope::Definition,
                conditions: vec![condition.clone()],
            },
            with(&|d| d.conditions = vec![condition.clone()]),
        ),
        (
            "conditions (slot)",
            SmartObjectDefOp::Conditions {
                scope: DefScope::Slot(1),
                conditions: vec![condition.clone()],
            },
            with(&|d| d.slots[1].conditions = vec![condition.clone()]),
        ),
        (
            "behaviors (definition)",
            SmartObjectDefOp::Behaviors {
                scope: DefScope::Definition,
                behaviors: vec![behavior.clone()],
            },
            with(&|d| d.behaviors = vec![behavior.clone()]),
        ),
        (
            "behaviors (slot)",
            SmartObjectDefOp::Behaviors {
                scope: DefScope::Slot(0),
                behaviors: vec![behavior.clone()],
            },
            with(&|d| d.slots[0].behaviors = vec![behavior.clone()]),
        ),
        (
            "slots: add",
            SmartObjectDefOp::AddSlot {
                name: "middle".into(),
            },
            with(&|d| d.slots.push(default_slot("middle"))),
        ),
        (
            "slots: remove",
            SmartObjectDefOp::RemoveSlot(0),
            with(&|d| {
                d.slots.remove(0);
            }),
        ),
        (
            "slots: rename",
            SmartObjectDefOp::RenameSlot {
                slot: 1,
                name: "east".into(),
            },
            with(&|d| d.slots[1].name = "east".into()),
        ),
        (
            "slots: enabled",
            SmartObjectDefOp::SlotEnabled {
                slot: 0,
                enabled: false,
            },
            with(&|d| d.slots[0].enabled = false),
        ),
        (
            "slots: initial tags",
            SmartObjectDefOp::SlotInitialTags {
                slot: 1,
                tags: vec!["State.Wet".into()],
            },
            with(&|d| d.slots[1].initial_tags = vec!["State.Wet".into()]),
        ),
        (
            "slots: offset",
            SmartObjectDefOp::SlotOffset { slot: 0, offset },
            with(&|d| d.slots[0].offset = offset),
        ),
        (
            "entrances: add",
            SmartObjectDefOp::AddEntrance {
                slot: 1,
                entrance: entrance.clone(),
            },
            with(&|d| d.slots[1].entrances.push(entrance.clone())),
        ),
        (
            "entrances: remove",
            SmartObjectDefOp::RemoveEntrance { slot: 0, index: 0 },
            with(&|d| d.slots[0].entrances.clear()),
        ),
        (
            "entrances: edit",
            SmartObjectDefOp::Entrance {
                slot: 0,
                index: 0,
                entrance: entrance.clone(),
            },
            with(&|d| d.slots[0].entrances[0] = entrance.clone()),
        ),
    ]
}

/// Acceptance (1): every section edited through its op saves exactly `to_ron` of the expected
/// definition, and the runtime loader reads those bytes back as that definition.
#[test]
fn each_section_op_saves_the_canonical_bytes_of_the_expected_definition() {
    for (name, op, expected) in section_cases() {
        let (mut world, file, _) = fixture(None);
        assert!(
            edit_def(&mut world, &op).expect(name),
            "{name}: the op changed the definition"
        );
        let written = save_def(&mut world).expect(name);
        let bytes = std::fs::read_to_string(&file).expect("read");
        let want = expected.to_ron().expect("serialize");
        assert_eq!(bytes, want, "{name}: saved bytes");
        assert_eq!(written, want.len(), "{name}");
        let loaded =
            SmartObjectDef::from_ron(&bytes).unwrap_or_else(|e| panic!("{name}: loader refused: {e}"));
        assert_eq!(loaded, expected, "{name}: the loader reads what was saved");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "{name}: one undo step"
        );
        assert!(
            !world
                .resource::<SmartObjectDefPanel>()
                .doc()
                .expect("doc")
                .dirty()
        );
    }
}

/// SplitMix64, for the seeded op sequence.
fn mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn random_op(state: &mut u64, def: &SmartObjectDef) -> SmartObjectDefOp {
    let n = |state: &mut u64, m: usize| (mix(state) % m.max(1) as u64) as usize;
    let slots = def.slots.len();
    let slot = n(state, slots + 1);
    let scope = if n(state, 3) == 0 {
        DefScope::Definition
    } else {
        DefScope::Slot(slot)
    };
    let f = |state: &mut u64| (mix(state) % 2000) as f32 / 1000.0 - 1.0;
    match n(state, 12) {
        0 => SmartObjectDefOp::Activity {
            scope,
            tags: vec![format!("Activity.T{}", n(state, 5))],
        },
        1 => SmartObjectDefOp::ActivityMerge(MERGES[n(state, 2)]),
        2 => SmartObjectDefOp::UserRule(RULES[n(state, 3)]),
        3 => SmartObjectDefOp::UserQuery {
            scope,
            query: (n(state, 2) == 0).then(|| ron::from_str("Has(\"Role.Guard\")").expect("query")),
        },
        4 => SmartObjectDefOp::Conditions {
            scope,
            conditions: vec![ConditionDef {
                path: format!("game::C{}", n(state, 4)),
                params: BTreeMap::new(),
            }],
        },
        5 => SmartObjectDefOp::Behaviors {
            scope,
            behaviors: vec![BehaviorDef {
                family: format!("f{}", n(state, 3)),
                data: "()".into(),
            }],
        },
        6 => SmartObjectDefOp::AddSlot {
            name: unused_slot_name(def),
        },
        7 => SmartObjectDefOp::RemoveSlot(slot),
        8 => SmartObjectDefOp::SlotOffset {
            slot,
            offset: OffsetDef {
                translation: (f(state), f(state), f(state)),
                rotation: rotation_of([f(state) * 180.0, 0.0, 0.0]),
            },
        },
        9 => SmartObjectDefOp::AddEntrance {
            slot,
            entrance: EntranceDef {
                offset: OffsetDef::default(),
                kind: KINDS[n(state, 3)],
                rank: n(state, 5) as i8,
            },
        },
        10 => SmartObjectDefOp::RemoveEntrance { slot, index: 0 },
        _ => SmartObjectDefOp::SlotEnabled {
            slot,
            enabled: n(state, 2) == 0,
        },
    }
}

/// Acceptance (1): a 50-op random sequence (including ops that name a slot or entrance that is not
/// there, refused and recorded as nothing), then undo to the bottom, returns the original bytes; a
/// save then writes nothing.
#[test]
fn fifty_random_ops_then_full_undo_return_the_original_bytes() {
    for seed in [1_u64, 7, 0x5eed] {
        let (mut world, file, text) = fixture(None);
        let mut state = seed;
        let mut applied = 0;
        for _ in 0..50 {
            let op = random_op(&mut state, &def_of(&world));
            if edit_def(&mut world, &op).unwrap_or(false) {
                applied += 1;
            }
        }
        assert!(
            applied > 20,
            "seed {seed}: most ops changed something ({applied})"
        );
        assert_ne!(
            def_of(&world),
            bench(),
            "seed {seed}: the sequence changed the definition"
        );
        let mut undone = 0;
        while crate::command::undo_world(&mut world) {
            undone += 1;
            assert!(undone <= 50, "seed {seed}: the stack ends");
        }
        let doc = world
            .resource::<SmartObjectDefPanel>()
            .doc()
            .expect("doc")
            .clone();
        assert_eq!(doc.def, bench(), "seed {seed}");
        assert_eq!(
            doc.def.to_ron().expect("serialize"),
            text,
            "seed {seed}: the original bytes"
        );
        assert!(!doc.dirty());
        assert_eq!(
            save_def(&mut world).expect("save"),
            0,
            "seed {seed}: nothing to write"
        );
        assert_eq!(std::fs::read_to_string(&file).expect("read"), text);
    }
}

/// The validator decides what is valid: a duplicate name is reported with its own message and
/// refuses Save, and the form itself does not stop the edit.
#[test]
fn the_runtime_validator_gates_save_with_its_own_message() {
    let (mut world, file, text) = fixture(None);
    edit_def(
        &mut world,
        &SmartObjectDefOp::RenameSlot {
            slot: 1,
            name: "left".into(),
        },
    )
    .expect("the form accepts the rename");
    let doc = world
        .resource::<SmartObjectDefPanel>()
        .doc()
        .expect("doc")
        .clone();
    let want = match doc.def.validate() {
        Err(e @ SmartObjectDefError::BadDefinition(_)) => e.to_string(),
        other => panic!("validator: {other:?}"),
    };
    assert_eq!(doc.validation().as_deref(), Some(want.as_str()));
    let refused = save_def(&mut world).expect_err("invalid");
    assert_eq!(refused.code, "smart_object_def_invalid");
    assert_eq!(refused.message, want);
    assert_eq!(
        std::fs::read_to_string(&file).expect("read"),
        text,
        "nothing written"
    );
    crate::command::undo_world(&mut world);
    assert!(
        world
            .resource::<SmartObjectDefPanel>()
            .doc()
            .expect("doc")
            .validation()
            .is_none()
    );
}

#[test]
fn a_missing_slot_or_entrance_is_a_bad_index_and_changes_nothing() {
    let (mut world, _, _) = fixture(None);
    for op in [
        SmartObjectDefOp::RemoveSlot(9),
        SmartObjectDefOp::RemoveEntrance { slot: 1, index: 0 },
        SmartObjectDefOp::Activity {
            scope: DefScope::Slot(5),
            tags: Vec::new(),
        },
    ] {
        let e = edit_def(&mut world, &op).expect_err("missing");
        assert_eq!(e.code, "smart_object_def_bad_index", "{op:?}");
    }
    assert_eq!(def_of(&world), bench());
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
}

#[test]
fn a_file_with_comments_warns_until_a_save_drops_them() {
    let text = format!(
        "// the bench by the door\n{}",
        bench().to_ron().expect("serialize")
    );
    let (mut world, file, _) = fixture(Some(&text));
    let doc = world
        .resource::<SmartObjectDefPanel>()
        .doc()
        .expect("doc")
        .clone();
    assert_eq!(doc.comments_warning(), Some(COMMENTS_WARNING));
    assert_eq!(
        save_def(&mut world).expect("clean"),
        0,
        "an unedited file keeps its comments"
    );
    assert_eq!(std::fs::read_to_string(&file).expect("read"), text);
    edit_def(&mut world, &SmartObjectDefOp::UserRule(UserRuleDef::SlotOnly)).expect("edit");
    let state = def_state(&mut world, &json!({})).expect("state");
    assert_eq!(state["comments_warning"], COMMENTS_WARNING);
    save_def(&mut world).expect("save");
    assert!(
        world
            .resource::<SmartObjectDefPanel>()
            .doc()
            .expect("doc")
            .comments_warning()
            .is_none()
    );
}

#[test]
fn opening_a_newer_or_broken_file_names_the_loader_reason() {
    let root = regia_test_utils::unique_temp_dir("smart-object-def-open");
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    std::fs::create_dir_all(root.join("assets")).expect("mkdir");
    std::fs::write(
        root.join("assets/new.smartobject.ron"),
        "SmartObjectDef(format_version: 2, slots: [])",
    )
    .expect("write");
    let e = open_def(&mut world, "assets/new.smartobject.ron").expect_err("newer");
    assert_eq!(e.code, "smart_object_def_parse_failed");
    assert!(e.message.contains("unsupported_version"), "{}", e.message);
    let e = open_def(&mut world, "assets/x.cue.ron").expect_err("suffix");
    assert_eq!(e.code, "smart_object_def_bad_path");
    let e = open_def(&mut world, "../outside.smartobject.ron").expect_err("outside");
    assert_eq!(e.code, "smart_object_def_bad_path");
}

#[test]
fn offset_drags_coalesce_into_one_undo_step() {
    let (mut world, _, _) = fixture(None);
    for i in 0..5 {
        let offset = OffsetDef {
            translation: (i as f32 * 0.1, 0.0, 0.0),
            ..OffsetDef::default()
        };
        edit_def(&mut world, &SmartObjectDefOp::SlotOffset { slot: 1, offset }).expect("edit");
    }
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    crate::command::undo_world(&mut world);
    assert_eq!(def_of(&world), bench());
}

#[test]
fn the_agent_handlers_edit_and_save_like_the_form() {
    let (mut world, file, _) = fixture(None);
    let edit = handler("editor.smart_object_def_edit");
    let state = handler("editor.smart_object_def_state");
    let out = edit(
        &mut world,
        &json!({ "op": "set_activity", "slot": "right", "tags": ["Activity.Lie"] }),
    )
    .expect("edit");
    assert_eq!(out["changed"], true);
    assert_eq!(out["slots"][1]["activity"], json!(["Activity.Lie"]));
    edit(
        &mut world,
        &json!({ "op": "set_rules", "activity_merge": "slot_first", "user_rule": "slot_only" }),
    )
    .expect("rules");
    edit(
        &mut world,
        &json!({ "op": "set_user_query", "query": "Has(\"Role.Guard\")" }),
    )
    .expect("query");
    edit(&mut world, &json!({ "op": "set_conditions", "slot": 0, "conditions": [{ "path": "g::Dry", "params": { "k": "v" } }] })).expect("conditions");
    edit(
        &mut world,
        &json!({ "op": "set_behaviors", "behaviors": [{ "family": "rest", "data": "()" }] }),
    )
    .expect("behaviors");
    edit(
        &mut world,
        &json!({ "op": "add_slot", "name": "middle", "translation": [0.0, 0.0, 0.2] }),
    )
    .expect("add slot");
    edit(
        &mut world,
        &json!({ "op": "set_slot", "slot": "middle", "enabled": false, "initial_tags": ["State.Wet"] }),
    )
    .expect("set slot");
    edit(&mut world, &json!({ "op": "add_entrance", "slot": 2, "translation": [0.0, 0.0, 1.0], "kind": "exit", "rank": 3 })).expect("entrance");
    edit(
        &mut world,
        &json!({ "op": "set_entrance", "slot": 0, "entrance": 0, "rank": -2 }),
    )
    .expect("set entrance");
    let saved = edit(&mut world, &json!({ "op": "save" })).expect("save");
    assert!(saved["written"].as_u64().is_some_and(|n| n > 0));
    let bytes = std::fs::read_to_string(&file).expect("read");
    let def = SmartObjectDef::from_ron(&bytes).expect("the loader reads it");
    assert_eq!(def.activity_merge, ActivityMergeDef::SlotFirst);
    assert_eq!(def.user_rule, UserRuleDef::SlotOnly);
    assert!(def.user_query.is_some());
    assert_eq!(def.slots[0].conditions[0].params["k"], "v");
    assert_eq!(def.behaviors[0].family, "rest");
    assert_eq!(def.slots[2].name, "middle");
    assert_eq!(def.slots[2].offset.translation, (0.0, 0.0, 0.2));
    assert!(!def.slots[2].enabled);
    assert_eq!(def.slots[2].entrances[0].kind, EntranceKindDef::Exit);
    assert_eq!(def.slots[0].entrances[0].rank, -2);
    let read = state(&mut world, &json!({})).expect("state");
    assert_eq!(read["dirty"], false);
    assert_eq!(read["valid"], true);
    assert_eq!(read["path"], REL);

    // All or nothing, and the reasons keep their codes.
    let reason = |e: RpcError| {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason").or_else(|| d.get("code")))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let before = def_of(&world);
    let e = edit(
        &mut world,
        &json!({ "op": "set_slot", "slot": "nope", "enabled": true }),
    )
    .expect_err("missing");
    assert_eq!(reason(e), "smart_object_def_bad_index");
    let e = edit(
        &mut world,
        &json!({ "op": "set_slot", "slot": 0, "name": "x", "translation": [1.0, "a", 0.0] }),
    )
    .expect_err("bad");
    assert_eq!(reason(e), "smart_object_def_bad_value");
    let e = edit(&mut world, &json!({ "op": "set_user_query", "query": "Has(" })).expect_err("bad query");
    assert_eq!(reason(e), "smart_object_def_bad_value");
    assert_eq!(def_of(&world), before, "a refused call changes nothing");
}

#[test]
fn without_an_open_definition_the_state_says_so_and_edits_are_refused() {
    let mut world = World::new();
    let state = def_state(&mut world, &json!({})).expect("state");
    assert_eq!(state["open"], false);
    assert!(def_edit(&mut world, &json!({ "op": "add_slot" })).is_err());
}

#[test]
fn the_panel_draws_a_definition_headless() {
    let (mut world, _, _) = fixture(None);
    let shapes = draw_frames(&mut world, 3, smart_object_def_panel);
    assert!(!shapes.is_empty());
    // Drawing changes nothing.
    assert_eq!(def_of(&world), bench());
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    let mut empty = World::new();
    assert!(!draw_frames(&mut empty, 2, smart_object_def_panel).is_empty());
}

#[test]
fn euler_rows_round_trip_a_yaw() {
    let r = rotation_of([90.0, 0.0, 0.0]);
    let e = euler_degrees(r);
    assert!(
        (e[0] - 90.0).abs() < 1e-3 && e[1].abs() < 1e-3 && e[2].abs() < 1e-3,
        "{e:?}"
    );
    assert_eq!(parse_query("").expect("empty"), None);
    assert_eq!(tags_of(" a, ,b "), vec!["a".to_owned(), "b".to_owned()]);
    assert_eq!(params_of("a=1; b = 2;bad")["b"], "2");
}

#[test]
fn the_autosave_snapshot_is_the_canonical_text_and_restores_dirty() {
    let (mut world, _, _) = fixture(None);
    assert!(
        (autosave::PROVIDER.dirty)(&world).is_empty(),
        "a clean definition is not autosaved"
    );
    edit_def(&mut world, &SmartObjectDefOp::UserRule(UserRuleDef::SlotOnly)).expect("edit");
    let snaps = (autosave::PROVIDER.snapshot)(&world);
    assert_eq!(snaps.len(), 1);
    let edited = def_of(&world);
    assert_eq!(snaps[0].bytes, edited.to_ron().expect("serialize").into_bytes());
    // A fresh editor restores the snapshot over the file: dirty, with the edit.
    let root = crate::mesh_source::project_root(&world).expect("root");
    let mut fresh = World::new();
    fresh.insert_resource(crate::agent_ext::EditorProject { root: Some(root) });
    (autosave::PROVIDER.restore)(&mut fresh, &snaps[0].key, &snaps[0].bytes).expect("restore");
    let doc = fresh
        .resource::<SmartObjectDefPanel>()
        .doc()
        .expect("doc")
        .clone();
    assert_eq!(doc.def, edited);
    assert!(doc.dirty());
}

/// Content ▸ New ▸ Smart object writes a definition the runtime loader reads and the editor opens
/// clean (no comments, nothing to save).
#[test]
fn content_new_smart_object_opens_clean_in_the_editor() {
    use crate::content::{NewAssetKind, create_asset};
    let root = regia_test_utils::unique_temp_dir("smart-object-def-new");
    let kind = NewAssetKind::parse("smart_object").expect("kind");
    assert!(NewAssetKind::ALL.contains(&kind), "in the New menu");
    assert_eq!(kind.label(), "Smart object");
    let path = create_asset(&root, kind, "assets/props/chair.smartobject.ron").expect("create");
    assert_eq!(path, "assets/props/chair.smartobject.ron");
    assert!(
        create_asset(&root, kind, "assets/props/chair.cue.ron").is_err(),
        "the extension is required"
    );
    let text = std::fs::read_to_string(root.join(&path)).expect("read");
    let def = SmartObjectDef::from_ron(&text).expect("the loader reads it");
    assert_eq!(def.slots, vec![default_slot("slot_0")]);
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject { root: Some(root) });
    open_def(&mut world, &path).expect("open");
    let doc = world
        .resource::<SmartObjectDefPanel>()
        .doc()
        .expect("doc")
        .clone();
    assert!(doc.validation().is_none() && doc.comments_warning().is_none() && !doc.dirty());
    assert_eq!(save_def(&mut world).expect("save"), 0);
}
