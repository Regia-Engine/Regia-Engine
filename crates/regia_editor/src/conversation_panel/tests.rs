// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests of the conversation graph editor (ADR-3298). Headless, no goldens: a fixture
//! project in a unique temp directory, the editor resource, the undo stack and the agent methods.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::edit::{Branch, ConvOp, NewOption, NewStep};
use super::graph::{EdgeKind, NodeKind};
use super::preview::PreviewState;
use super::{ConvEditor, edit, move_node};

/// The hand-written fixture (not in the serializer's canonical form on purpose).
const GATE: &str = r#"(
    format_version: 1,
    id: "gate",
    notes: "Gate scene.",
    lines: [
        (id: "greet", speaker: "guard", text: "gate-greet"),
        (id: "ask", speaker: "guard", text: "gate-ask"),
        (id: "yes", speaker: "hero", text: "gate-yes"),
        (id: "no", speaker: "hero", text: "gate-no"),
        (id: "warn", speaker: "guard", text: "gate-warn"),
        (id: "bye", speaker: "guard", text: "gate-bye"),
    ],
    steps: [
        Line("greet"),
        If(cond: "met_captain", then: "short", else: "long"),
        Label("long"),
        Line("ask"),
        Choice(options: [
            (text: "gate-opt-yes", goto: "agree"),
            (text: "gate-opt-no", cond: Some("brave"), goto: "refuse"),
        ]),
        Label("agree"),
        Line("yes"),
        Event("open_gate"),
        Goto("short"),
        Label("refuse"),
        Line("no"),
        Line("warn"),
        End,
        Label("short"),
        Line("bye"),
        End,
    ],
)
"#;

/// Seeded faults: an unreachable label, a missing goto target, a choice without options, a line
/// without a voice clip, and an over-long line.
const FAULTS: &str = r#"(
    format_version: 1,
    id: "faults",
    lines: [
        (id: "a", speaker: "guard", text: "faults-a"),
        (id: "long", speaker: "guard", text: "faults-long"),
        (id: "b", speaker: "guard", text: "faults-b"),
    ],
    steps: [
        Line("a"),
        Line("long"),
        If(cond: "x", then: "pick", else: "jump"),
        Label("pick"),
        Choice(options: []),
        Label("jump"),
        Goto("nowhere"),
        End,
        Label("orphan"),
        Line("b"),
    ],
)
"#;

/// A one-second silent mono WAV.
fn wav() -> Vec<u8> {
    let rate: u32 = 8000;
    let data = vec![0u8; rate as usize * 2];
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dir");
    }
    std::fs::write(path, bytes).expect("write");
}

/// A project with speakers, settings, two locales, clips and both fixtures. Returns the root.
fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    let conv = root.join("assets/conversations");
    write(
        &conv.join("speakers.ron"),
        br#"(format_version: 1, speakers: {"guard": (), "hero": ()})"#,
    );
    write(
        &conv.join("conversation.ron"),
        br#"(format_version: 1, voice_path: "audio/vo/{locale}/{conversation}_{line}.wav")"#,
    );
    write(&conv.join("gate.conv.ron"), GATE.as_bytes());
    write(&conv.join("faults.conv.ron"), FAULTS.as_bytes());
    let long = "word ".repeat(80);
    let en = format!(
        "gate-greet = Halt.\ngate-ask = Business?\ngate-yes = Trade.\ngate-no = None of yours.\n\
         gate-warn = Then go.\ngate-bye = Pass.\ngate-opt-yes = Say yes\ngate-opt-no = Refuse\n\
         faults-a = Hello\nfaults-long = {}\nfaults-b = Unseen\n",
        long.trim()
    );
    write(&root.join("assets/localization/en/main.ftl"), en.as_bytes());
    write(
        &root.join("assets/localization/de/main.ftl"),
        "gate-greet = Halt.\nfaults-a = Hallo\n".as_bytes(),
    );
    for line in ["greet", "ask", "yes", "no", "warn", "bye"] {
        write(&root.join(format!("assets/audio/vo/en/gate_{line}.wav")), &wav());
    }
    for line in ["long", "b"] {
        write(
            &root.join(format!("assets/audio/vo/en/faults_{line}.wav")),
            &wav(),
        );
    }
    root
}

fn world() -> World {
    let mut world = World::new();
    world.insert_resource(crate::command::UndoStack::new());
    world
}

fn open(world: &mut World, root: &Path, name: &str) -> PathBuf {
    let path = root.join("assets/conversations").join(name);
    world
        .get_resource_or_insert_with(ConvEditor::default)
        .open(&path, Some(root))
        .expect("opens");
    path
}

fn editor(world: &World) -> &ConvEditor {
    world.resource::<ConvEditor>()
}

fn ids(world: &World) -> Vec<String> {
    editor(world).graph.nodes.iter().map(|n| n.id.clone()).collect()
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = super::agent::methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .unwrap_or_else(|| panic!("no method {method}"));
    handler(world, &params)
}

fn reason(err: &regia_agent::protocol::RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or("")
        .to_string()
}

#[test]
fn graph_of_the_fixture_has_the_expected_nodes_and_edges() {
    let root = project("conv-graph");
    let mut world = world();
    open(&mut world, &root, "gate.conv.ron");
    assert_eq!(
        ids(&world),
        [
            "line:greet",
            "if:1",
            "label:long",
            "choice:1",
            "label:agree",
            "label:refuse",
            "end:1",
            "label:short",
            "end:2"
        ]
    );
    let g = &editor(&world).graph;
    let kinds: Vec<NodeKind> = g.nodes.iter().map(|n| n.kind).collect();
    assert_eq!(kinds[1], NodeKind::If);
    assert_eq!(kinds[3], NodeKind::Choice);
    assert_eq!(kinds[6], NodeKind::End);
    let edges: Vec<(String, Option<String>, EdgeKind)> = g
        .edges
        .iter()
        .map(|e| {
            (
                g.nodes[e.from].id.clone(),
                e.to.map(|t| g.nodes[t].id.clone()),
                e.kind,
            )
        })
        .collect();
    let s = |a: &str| a.to_string();
    assert_eq!(
        edges,
        vec![
            (s("line:greet"), Some(s("if:1")), EdgeKind::Next),
            (s("if:1"), Some(s("label:short")), EdgeKind::Then),
            (s("if:1"), Some(s("label:long")), EdgeKind::Else),
            (s("label:long"), Some(s("choice:1")), EdgeKind::Next),
            (s("choice:1"), Some(s("label:agree")), EdgeKind::Option),
            (s("choice:1"), Some(s("label:refuse")), EdgeKind::Option),
            (s("label:agree"), Some(s("label:short")), EdgeKind::Goto),
            (s("label:refuse"), Some(s("end:1")), EdgeKind::Next),
            (s("label:short"), Some(s("end:2")), EdgeKind::Next),
        ]
    );
    // The second option carries its condition.
    assert_eq!(g.edges[5].cond.as_deref(), Some("brave"));
    assert_eq!(g.edges[5].option, Some(1));
    // Layered: the entry is on top, each layer below its parents.
    let layout = &editor(&world).layout;
    assert_eq!(layout.layers[0], 0);
    assert!(layout.layers.iter().all(|&l| l <= 6));
    assert!(layout.positions[1].1 > layout.positions[0].1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn layout_positions_are_stable_when_one_line_changes() {
    let root = project("conv-layout");
    let mut world = world();
    open(&mut world, &root, "gate.conv.ron");
    let before = editor(&world).layout.clone();
    edit(
        &mut world,
        "text",
        None,
        &[ConvOp::SetLine {
            line: "ask".into(),
            text: Some("gate-ask-rewritten-with-a-much-longer-key".into()),
            speaker: Some("hero".into()),
            note: Some("rewritten".into()),
            emotion: Some("tired".into()),
        }],
    )
    .expect("edits");
    assert_ne!(editor(&world).text(), GATE, "the edit changed the file");
    assert_eq!(editor(&world).layout, before, "no node moved");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn each_edit_plus_undo_restores_identical_bytes() {
    let root = project("conv-undo");
    let mut world = world();
    open(&mut world, &root, "gate.conv.ron");
    let ops: Vec<ConvOp> = vec![
        ConvOp::AddLine {
            id: "extra".into(),
            speaker: "guard".into(),
            text: "gate-extra".into(),
            at: Some(1),
        },
        ConvOp::AddStep {
            step: NewStep::Event { name: "shout".into() },
            at: None,
        },
        ConvOp::AddStep {
            step: NewStep::Choice {
                prompt: Some("gate-prompt".into()),
                options: vec![NewOption {
                    text: "gate-o".into(),
                    cond: None,
                    goto: "long".into(),
                }],
            },
            at: Some(0),
        },
        ConvOp::DeleteStep { at: 3 },
        ConvOp::DeleteStep { at: 0 },
        ConvOp::MoveStep { from: 0, to: 2 },
        ConvOp::SetLine {
            line: "greet".into(),
            text: Some("gate-hello".into()),
            speaker: None,
            note: None,
            emotion: None,
        },
        ConvOp::SetPrompt {
            at: 4,
            prompt: "gate-prompt".into(),
        },
        ConvOp::AddOption {
            at: 4,
            text: "gate-opt-maybe".into(),
            cond: Some("unsure".into()),
            goto: "short".into(),
        },
        ConvOp::SetOption {
            at: 4,
            option: 1,
            text: None,
            cond: Some(String::new()),
            goto: Some("agree".into()),
        },
        ConvOp::DeleteOption { at: 4, option: 0 },
        ConvOp::MoveOption {
            at: 4,
            from: 1,
            to: 0,
        },
        ConvOp::SetCond {
            at: 1,
            cond: "knows_guard".into(),
        },
        ConvOp::SetEvent {
            at: 7,
            name: "gate_opens".into(),
        },
        ConvOp::Retarget {
            at: 8,
            branch: Branch::Goto,
            option: None,
            label: "refuse".into(),
        },
        ConvOp::Retarget {
            at: 1,
            branch: Branch::Else,
            option: None,
            label: "agree".into(),
        },
        ConvOp::Retarget {
            at: 4,
            branch: Branch::Option,
            option: Some(1),
            label: "short".into(),
        },
        ConvOp::RenameLabel {
            from: "long".into(),
            to: "longer".into(),
        },
    ];
    for op in ops {
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        edit(&mut world, "op", None, std::slice::from_ref(&op)).unwrap_or_else(|e| panic!("{op:?}: {e}"));
        let after = editor(&world).text().to_string();
        assert_ne!(after, GATE, "{op:?} changed nothing");
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1,
            "{op:?} is one undo step"
        );
        crate::command::undo_world(&mut world);
        assert_eq!(editor(&world).text(), GATE, "{op:?} + undo");
        assert!(!editor(&world).dirty());
        crate::command::redo_world(&mut world);
        assert_eq!(editor(&world).text(), after, "{op:?} + undo + redo");
        crate::command::undo_world(&mut world);
    }
    // A layout move is undoable too and never touches the file text.
    move_node(&mut world, "label:long", Some((900.0, 40.0))).expect("moves");
    move_node(&mut world, "label:long", Some((910.0, 50.0))).expect("moves");
    assert_eq!(editor(&world).text(), GATE);
    let i = editor(&world).graph.node_index("label:long").expect("node");
    assert_eq!(editor(&world).layout.positions[i], (910.0, 50.0));
    crate::command::undo_world(&mut world);
    assert!(
        editor(&world).sidecar().positions.is_empty(),
        "one drag, one step"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_refused_op_changes_nothing_and_adds_no_undo_step() {
    let root = project("conv-refused");
    let mut world = world();
    open(&mut world, &root, "gate.conv.ron");
    let cases: Vec<(ConvOp, &str)> = vec![
        (
            ConvOp::AddLine {
                id: "greet".into(),
                speaker: "guard".into(),
                text: "k".into(),
                at: None,
            },
            "duplicate_line",
        ),
        (ConvOp::DeleteStep { at: 99 }, "bad_step"),
        (
            ConvOp::SetCond {
                at: 0,
                cond: "x".into(),
            },
            "wrong_step",
        ),
        (
            ConvOp::RenameLabel {
                from: "long".into(),
                to: "short".into(),
            },
            "duplicate_label",
        ),
        (
            ConvOp::RenameLabel {
                from: "nope".into(),
                to: "x".into(),
            },
            "unknown_label",
        ),
        (
            ConvOp::AddStep {
                step: NewStep::Line { line: "ghost".into() },
                at: None,
            },
            "unknown_line",
        ),
        (
            ConvOp::Retarget {
                at: 4,
                branch: Branch::Option,
                option: Some(7),
                label: "long".into(),
            },
            "bad_option",
        ),
    ];
    for (op, want) in cases {
        // The first op is fine; the batch is refused as a whole.
        let ok = ConvOp::SetEvent {
            at: 7,
            name: "x".into(),
        };
        let err = edit(&mut world, "bad", None, &[ok, op.clone()]).expect_err("refused");
        assert_eq!(err.reason(), want, "{op:?}");
        assert_eq!(editor(&world).text(), GATE);
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 0);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn label_rename_updates_every_jump_as_one_step() {
    let root = project("conv-rename");
    let mut world = world();
    open(&mut world, &root, "gate.conv.ron");
    edit(
        &mut world,
        "rename",
        None,
        &[ConvOp::RenameLabel {
            from: "short".into(),
            to: "quick".into(),
        }],
    )
    .expect("renames");
    let text = editor(&world).text().to_string();
    assert!(!text.contains("\"short\""), "{text}");
    let asset = editor(&world).asset.clone().expect("parses");
    use regia_framework::conversation::Step;
    assert!(asset.steps.contains(&Step::Label("quick".into())));
    assert!(asset.steps.contains(&Step::Goto("quick".into())));
    assert!(
        asset
            .steps
            .iter()
            .any(|s| matches!(s, Step::If { then, .. } if then == "quick"))
    );
    // Every edge that went to `short` now reaches `quick`, and no jump dangles.
    let g = &editor(&world).graph;
    assert!(g.edges.iter().all(|e| e.to.is_some()));
    assert_eq!(
        g.edges
            .iter()
            .filter(|e| e.to.map(|t| g.nodes[t].id.as_str()) == Some("label:quick"))
            .count(),
        2
    );
    assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 1);
    crate::command::undo_world(&mut world);
    assert_eq!(editor(&world).text(), GATE);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn validation_finds_each_seeded_fault_on_its_node() {
    let root = project("conv-validate");
    let mut world = world();
    open(&mut world, &root, "faults.conv.ron");
    let e = editor(&world);
    let found: Vec<(&str, Option<&str>, &str)> = e
        .issues
        .iter()
        .map(|f| {
            (
                f.issue.code,
                f.node.map(|n| e.graph.nodes[n].id.as_str()),
                f.issue.locale.as_str(),
            )
        })
        .collect();
    let has = |code: &str, node: &str| found.iter().any(|(c, n, _)| *c == code && *n == Some(node));
    assert!(has("conv_unreachable", "label:orphan"), "{found:?}");
    assert!(
        has("conv_bad_label", "label:jump"),
        "missing goto target: {found:?}"
    );
    assert!(has("conv_empty_choice", "choice:1"), "{found:?}");
    assert!(has("conv_line_too_long", "line:a"), "{found:?}");
    let missing: Vec<&str> = found
        .iter()
        .filter(|(c, n, _)| *c == "conv_missing_clip" && *n == Some("line:a"))
        .map(|(_, _, l)| *l)
        .collect();
    assert_eq!(missing, ["de", "en"], "line `a` has no clip in any locale");
    // Lines with a clip are not reported missing.
    assert!(
        !e.issues
            .iter()
            .any(|f| f.issue.code == "conv_missing_clip" && f.issue.line == "long")
    );
    // The agent lists the same findings with node ids.
    let graph = call(&mut world, "editor.conversation_graph", json!({})).expect("graph");
    let codes: Vec<&str> = graph["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter_map(|i| i["code"].as_str())
        .collect();
    for code in [
        "conv_unreachable",
        "conv_bad_label",
        "conv_empty_choice",
        "conv_missing_clip",
        "conv_line_too_long",
    ] {
        assert!(codes.contains(&code), "{code}: {codes:?}");
    }
    let choice = graph["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["id"] == "choice:1")
        .expect("choice node");
    assert_eq!(choice["issues"], json!(["conv_empty_choice"]));
    let _ = std::fs::remove_dir_all(&root);
}

fn state(world: &World) -> PreviewState {
    editor(world).preview.as_ref().expect("a preview").state.clone()
}

fn line_of(world: &World) -> String {
    match state(world) {
        PreviewState::Line { line, .. } => line,
        other => panic!("not on a line: {other:?}"),
    }
}

#[test]
fn preview_follows_choices_and_conditions() {
    let root = project("conv-preview");
    let mut world = world();
    open(&mut world, &root, "gate.conv.ron");
    // met_captain unknown (false): the long way, one offered option (brave unknown).
    world.resource_mut::<ConvEditor>().preview_from(0, false);
    assert_eq!(line_of(&world), "greet");
    let voice = editor(&world).preview_voice.clone();
    assert_eq!(
        voice,
        Some(("audio/vo/en/gate_greet.wav".to_string(), false)),
        "the clip resolves; not played in tests"
    );
    let step = |world: &mut World, choose| world.resource_mut::<ConvEditor>().preview_step(choose, false);
    step(&mut world, None).expect("next");
    assert_eq!(line_of(&world), "ask");
    step(&mut world, None).expect("next");
    match state(&world) {
        PreviewState::Choice { offered, .. } => {
            assert_eq!(offered.len(), 1);
            assert_eq!(offered[0].2, "agree");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(step(&mut world, None), Err("choice_required"));
    assert_eq!(step(&mut world, Some(3)), Err("choice_out_of_range"));
    step(&mut world, Some(0)).expect("choose");
    assert_eq!(line_of(&world), "yes");
    step(&mut world, None).expect("next");
    assert_eq!(line_of(&world), "bye");
    assert_eq!(
        editor(&world).preview.as_ref().map(|p| p.events.clone()),
        Some(vec!["open_gate".to_string()])
    );
    step(&mut world, None).expect("next");
    assert!(matches!(
        state(&world),
        PreviewState::Finished {
            reason: "finished",
            ..
        }
    ));
    // brave true: both options; the second leads to the refusal.
    world.resource_mut::<ConvEditor>().conditions = BTreeMap::from([("brave".to_string(), true)]);
    let ask = editor(&world).graph.node_index("label:long").expect("node");
    world.resource_mut::<ConvEditor>().preview_from(ask, false);
    assert_eq!(line_of(&world), "ask");
    step(&mut world, None).expect("next");
    step(&mut world, Some(1)).expect("choose refuse");
    assert_eq!(line_of(&world), "no");
    step(&mut world, None).expect("next");
    assert_eq!(line_of(&world), "warn");
    // met_captain true: the short way.
    world
        .resource_mut::<ConvEditor>()
        .conditions
        .insert("met_captain".into(), true);
    world.resource_mut::<ConvEditor>().preview_from(0, false);
    step(&mut world, None).expect("next");
    assert_eq!(line_of(&world), "bye");
    // The agent drives the same preview and names the current node.
    let out = call(
        &mut world,
        "editor.conversation_preview_step",
        json!({ "start": "", "conditions": { "met_captain": false, "brave": true } }),
    )
    .expect("start");
    assert_eq!(out["state"], "line");
    assert_eq!(out["node"], "line:greet");
    assert_eq!(out["line"]["display"], "Halt.");
    call(&mut world, "editor.conversation_preview_step", json!({})).expect("next");
    let out = call(&mut world, "editor.conversation_preview_step", json!({})).expect("next");
    assert_eq!(out["state"], "choice");
    assert_eq!(out["node"], "choice:1");
    assert_eq!(out["options"][1]["display"], "Refuse");
    let out = call(
        &mut world,
        "editor.conversation_preview_step",
        json!({ "choose": 1 }),
    )
    .expect("choose");
    assert_eq!(out["line"]["id"], "no");
    let err = call(
        &mut world,
        "editor.conversation_preview_step",
        json!({ "choose": 0 }),
    )
    .expect_err("no choice waits");
    assert_eq!(reason(&err), "no_choice_pending");
    let out = call(
        &mut world,
        "editor.conversation_preview_step",
        json!({ "stop": true }),
    )
    .expect("stop");
    assert_eq!(out["state"], "stopped");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_goto_loop_stops_the_preview_at_the_step_budget() {
    let root = project("conv-loop");
    let path = root.join("assets/conversations/loop.conv.ron");
    write(
        &path,
        br#"(id: "loop", lines: [], steps: [Label("spin"), Goto("spin")])"#,
    );
    let mut world = world();
    open(&mut world, &root, "loop.conv.ron");
    world.resource_mut::<ConvEditor>().preview_from(0, false);
    assert!(matches!(
        state(&world),
        PreviewState::Finished {
            reason: "step_budget",
            ..
        }
    ));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn open_and_save_without_edits_is_byte_identical() {
    let root = project("conv-roundtrip");
    let mut world = world();
    // A hand-written file keeps its bytes.
    let path = open(&mut world, &root, "gate.conv.ron");
    world.resource_mut::<ConvEditor>().save(None).expect("saves");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), GATE);
    // A file in the canonical form keeps its bytes, and an edit writes that form.
    let asset = editor(&world).asset.clone().expect("asset");
    let canonical = regia_framework::conversation::asset::to_ron(&asset).expect("ron");
    let canon_path = root.join("assets/conversations/canon.conv.ron");
    write(&canon_path, canonical.as_bytes());
    open(&mut world, &root, "canon.conv.ron");
    world.resource_mut::<ConvEditor>().save(None).expect("saves");
    assert_eq!(std::fs::read_to_string(&canon_path).expect("read"), canonical);
    // Edit and undo, then save: the bytes are still the same.
    edit(
        &mut world,
        "x",
        None,
        &[ConvOp::SetEvent {
            at: 7,
            name: "y".into(),
        }],
    )
    .expect("edit");
    crate::command::undo_world(&mut world);
    world.resource_mut::<ConvEditor>().save(None).expect("saves");
    assert_eq!(std::fs::read_to_string(&canon_path).expect("read"), canonical);
    // An edit's file is the serializer's canonical form of the edited asset.
    edit(
        &mut world,
        "x",
        None,
        &[ConvOp::SetEvent {
            at: 7,
            name: "y".into(),
        }],
    )
    .expect("edit");
    let edited = editor(&world).asset.clone().expect("asset");
    assert_eq!(
        editor(&world).text(),
        regia_framework::conversation::asset::to_ron(&edited).expect("ron")
    );
    // A wrong extension is refused.
    let err = world
        .resource_mut::<ConvEditor>()
        .save(Some(&root.join("x.ron")))
        .expect_err("refused");
    assert!(err.contains(".conv.ron"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_malformed_asset_opens_read_only_with_the_parse_error() {
    let root = project("conv-malformed");
    let path = root.join("assets/conversations/broken.conv.ron");
    write(
        &path,
        b"(\n    id: \"broken\",\n    lines: [ (id: \"a\", speaker: ),\n]\n",
    );
    let mut world = world();
    let out = call(
        &mut world,
        "editor.conversation_open",
        json!({ "path": path.display().to_string(), "project": root.display().to_string() }),
    )
    .expect("opens read-only");
    assert_eq!(out["read_only"], true);
    let error = out["parse_error"].as_str().expect("an error").to_string();
    assert!(error.starts_with("broken.conv.ron:3:"), "{error}");
    let err = edit(
        &mut world,
        "x",
        None,
        &[ConvOp::RenameLabel {
            from: "a".into(),
            to: "b".into(),
        }],
    )
    .expect_err("read-only");
    assert_eq!(err.reason(), "read_only");
    let err = call(
        &mut world,
        "editor.conversation_edit",
        json!({ "ops": [{ "op": "delete_step", "at": 0 }] }),
    )
    .expect_err("read-only");
    assert_eq!(reason(&err), "read_only");
    let graph = call(&mut world, "editor.conversation_graph", json!({})).expect("graph");
    assert_eq!(graph["nodes"], json!([]));
    assert_eq!(graph["parse_error"], json!(error));
    // The file is untouched.
    assert!(
        std::fs::read_to_string(&path)
            .expect("read")
            .contains("speaker: )")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_finds_nodes_by_text_label_and_key() {
    let root = project("conv-search");
    let mut world = world();
    open(&mut world, &root, "gate.conv.ron");
    let found = |world: &World, q: &str| -> Vec<String> {
        let e = editor(world);
        e.search(q)
            .into_iter()
            .map(|n| e.graph.nodes[n].id.clone())
            .collect()
    };
    assert_eq!(found(&world, "none of yours"), ["label:refuse"], "resolved text");
    assert_eq!(
        found(&world, "AGREE"),
        ["choice:1", "label:agree"],
        "label, any case"
    );
    assert_eq!(found(&world, "gate-bye"), ["label:short"], "text key");
    assert_eq!(found(&world, "open_gate"), ["label:agree"], "event");
    assert!(found(&world, "").is_empty());
    let out = call(
        &mut world,
        "editor.conversation_graph",
        json!({ "search": "pass" }),
    )
    .expect("graph");
    assert_eq!(out["matches"], json!(["label:short"]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn moved_nodes_persist_in_the_sidecar_not_the_asset() {
    let root = project("conv-sidecar");
    let mut world = world();
    let path = open(&mut world, &root, "gate.conv.ron");
    let out = call(
        &mut world,
        "editor.conversation_edit",
        json!({ "move": { "node": "end:2", "x": 1200.0, "y": 80.0 } }),
    )
    .expect("move");
    assert_eq!(out["dirty"], true);
    let saved = call(&mut world, "editor.conversation_save", json!({})).expect("save");
    let sidecar = saved["sidecar"].as_str().expect("a sidecar").to_string();
    assert!(sidecar.contains(".regia"), "{sidecar}");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), GATE);
    let mut fresh = self::world();
    open(&mut fresh, &root, "gate.conv.ron");
    let e = editor(&fresh);
    let i = e.graph.node_index("end:2").expect("node");
    assert_eq!(e.layout.positions[i], (1200.0, 80.0));
    assert!(e.layout.pinned[i]);
    // Reset returns it to the automatic place.
    call(
        &mut fresh,
        "editor.conversation_edit",
        json!({ "move": { "node": "end:2", "reset": true } }),
    )
    .expect("reset");
    assert!(!editor(&fresh).layout.pinned[i]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn agent_edits_are_undoable_and_errors_name_a_reason() {
    let root = project("conv-agent");
    let mut world = world();
    let err = call(&mut world, "editor.conversation_graph", json!({})).expect_err("nothing open");
    assert_eq!(reason(&err), "no_conversation_open");
    let path = root.join("assets/conversations/gate.conv.ron");
    let out = call(
        &mut world,
        "editor.conversation_open",
        json!({ "path": path.display().to_string() }),
    )
    .expect("open");
    assert_eq!(out["id"], "gate");
    assert_eq!(out["nodes"], 9);
    assert_eq!(out["edges"], 9);
    let out = call(
        &mut world,
        "editor.conversation_edit",
        json!({ "label": "branch", "ops": [
            { "op": "add_line", "id": "think", "speaker": "hero", "text": "gate-think", "at": 1 },
            { "op": "retarget", "at": 2, "branch": "then", "label": "long" },
            { "op": "add_option", "at": 5, "text": "gate-opt-wait", "goto": "short" },
        ], "select": "line:greet" }),
    )
    .expect("edit");
    assert_eq!(out["selected"], "line:greet");
    let graph = call(&mut world, "editor.conversation_graph", json!({ "text": true })).expect("graph");
    assert!(graph["text"].as_str().expect("text").contains("gate-think"));
    let options = graph["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .filter(|e| e["kind"] == "option")
        .count();
    assert_eq!(options, 3);
    crate::command::undo_world(&mut world);
    assert_eq!(editor(&world).text(), GATE, "one undo reverts the batch");
    for (params, want) in [
        (json!({ "ops": [{ "op": "explode" }] }), "bad_ops"),
        (json!({ "ops": [{ "op": "delete_step", "at": 40 }] }), "bad_step"),
        (json!({ "select": "nope" }), "bad_node"),
        (json!({}), "missing_param"),
        (json!({ "move": { "node": "end:1" } }), "bad_move"),
    ] {
        let err = call(&mut world, "editor.conversation_edit", params).expect_err("refused");
        assert_eq!(reason(&err), want);
    }
    let err = call(&mut world, "editor.conversation_open", json!({ "path": "x.ron" })).expect_err("bad path");
    assert_eq!(reason(&err), "bad_path");
    let err = call(&mut world, "editor.conversation_preview_step", json!({})).expect_err("no preview");
    assert_eq!(reason(&err), "no_preview");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn method_names_are_unique_and_namespaced() {
    let names: Vec<&str> = super::agent::methods().iter().map(|(s, _)| s.name).collect();
    let unique: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(unique.len(), names.len());
    assert!(names.iter().all(|n| n.starts_with("editor.conversation_")));
}

fn draw_frames(world: &mut World, ctx: &egui::Context, frames: usize) {
    for _ in 0..frames {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| super::panel::conversation_panel(ui, world));
        output.textures_delta.clear();
    }
}

#[test]
fn the_panel_draws_headless_without_editing() {
    let root = project("conv-panel");
    let mut world = world();
    let ctx = egui::Context::default();
    // Nothing open, then a graph with a selection, findings and a preview, then a read-only file.
    draw_frames(&mut world, &ctx, 2);
    open(&mut world, &root, "faults.conv.ron");
    world.resource_mut::<ConvEditor>().selected = Some("choice:1".into());
    world.resource_mut::<ConvEditor>().preview_from(0, false);
    world.resource_mut::<ConvEditor>().search = "hello".into();
    let before = editor(&world).text().to_string();
    draw_frames(&mut world, &ctx, 3);
    assert_eq!(editor(&world).text(), before);
    assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 0);
    assert!(editor(&world).ui.has_view(), "the canvas laid out a view");
    write(&root.join("assets/conversations/bad.conv.ron"), b"(id: ");
    open(&mut world, &root, "bad.conv.ron");
    draw_frames(&mut world, &ctx, 2);
    assert!(editor(&world).read_only());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn panel_registration_has_a_stable_id() {
    let panel = crate::shell::Panel::Conversation;
    assert_eq!(panel.id(), "conversation");
    assert_eq!(crate::shell::Panel::from_id("conversation"), Some(panel));
    assert_eq!(panel.title(), "Conversation");
}

/// FU-5: the Nodes list selects a node (Enter), moves it (Alt+Right) and retargets its `then`
/// jump to the `refuse` label (Connect…: from output "then", to node "refuse") with keys only;
/// the asset and layout equal the ones the mouse's drag and port drag build (`move_node`, the
/// `retarget` op).
#[test]
fn the_nodes_list_edits_by_keyboard_what_the_mouse_edits() {
    use crate::canvas_keys::test_input::{frame, key, labels_of, tab_to, text};
    use egui::{Key, Modifiers};
    const SIZE: [f32; 2] = [1400.0, 900.0];
    let root = project("conv-keys");
    let mut mouse = world();
    open(&mut mouse, &root, "gate.conv.ron");
    let i = editor(&mouse).graph.node_index("if:1").expect("if node");
    let (x, y) = editor(&mouse).layout.positions[i];
    move_node(&mut mouse, "if:1", Some((x + crate::canvas_keys::GRID_STEP, y))).expect("move");
    edit(
        &mut mouse,
        "retarget to refuse",
        None,
        &[ConvOp::Retarget {
            at: 1,
            branch: Branch::Then,
            option: None,
            label: "refuse".into(),
        }],
    )
    .expect("retarget");

    let mut world = world();
    open(&mut world, &root, "gate.conv.ron");
    let ctx = egui::Context::default();
    let draw = |world: &mut World, events: Vec<egui::Event>| {
        frame(&ctx, SIZE, events, |ui| {
            super::panel::conversation_panel(ui, world)
        })
    };
    let first = draw(&mut world, vec![]);
    assert!(
        labels_of(&first, egui::accesskit::Role::Canvas)
            .contains(&"Conversation canvas, 9 nodes".to_string()),
        "{:?}",
        labels_of(&first, egui::accesskit::Role::Canvas)
    );
    let row = super::panel::node_row_name(editor(&world), i);
    tab_to(&ctx, SIZE, &row, false, 400, |ui| {
        super::panel::conversation_panel(ui, &mut world)
    });
    draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
    assert_eq!(editor(&world).selected.as_deref(), Some("if:1"), "Enter selects");
    draw(&mut world, vec![key(Key::ArrowRight, Modifiers::ALT)]);
    // Ctrl+Shift+C on the row starts the connection (the Connect… form, its first field focused).
    draw(
        &mut world,
        vec![key(Key::C, Modifiers::COMMAND | Modifiers::SHIFT)],
    );
    draw(&mut world, vec![]);
    draw(&mut world, vec![text("then")]);
    draw(&mut world, vec![key(Key::Tab, Modifiers::NONE)]);
    draw(&mut world, vec![text("refuse")]);
    draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
    draw(&mut world, vec![]);
    assert_eq!(editor(&world).toast, editor(&mouse).toast, "no edit was refused");
    assert_eq!(editor(&world).text(), editor(&mouse).text(), "same asset");
    assert_eq!(
        editor(&world).layout.positions,
        editor(&mouse).layout.positions,
        "same layout"
    );
    assert_eq!(editor(&world).layout.pinned, editor(&mouse).layout.pinned);
    let _ = std::fs::remove_dir_all(&root);
}
