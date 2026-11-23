// SPDX-License-Identifier: MPL-2.0
//! State tree authoring from the panel (spec 38 § 4): ambient tasks, includes (add, change, list the root,
//! remove), the tree-level fields and a state's utility score. Each operation is driven by clicking the drawn
//! control, saved, and compared with the agent path: `ai.statetree_edit` applies the same ops with
//! `state_tree_edit::apply_ops` and writes `state_tree_edit::to_canonical_ron` (regia_agent
//! `state_tree.rs`, `statetree_edit`); the handler is private to regia_agent, so the comparison calls the
//! same two functions on the same file.

use std::path::{Path, PathBuf};

use regia_ai::state_tree::{StateTreeDef, UtilityDef};
use regia_ai::state_tree_edit::{self, StOp, ValidateOptions};
use serde_json::json;

use super::*;
use crate::command::UndoStack;

const GUARD: &str = r#"(format_version: 2, root: "Root", states: [
    (name: "Root", children: ["Idle", "Alert"]),
    (name: "Idle", utility: Some((key: "calm", scale: 1.0, bias: 0.25)),
        transitions: [(trigger: Event("heard"), target: State("Alert"))]),
    (name: "Alert", transitions: [(trigger: Event("calm"), target: State("Idle"))]),
], ambient: ["ai/listen", "ai/look"])"#;

const SUB: &str = r#"(format_version: 2, root: "Fight", states: [(name: "Fight")])"#;

const FILE: &str = "guard.statetree.ron";

/// A project with the fixture tree (written canonically) and an included tree, the state tree editor open
/// on it, and the edit world's project root set (the validator reads includes from it).
fn project(label: &str) -> (PathBuf, World) {
    let root = regia_test_utils::unique_temp_dir(label);
    std::fs::create_dir_all(root.join("ai")).expect("dir");
    let def = state_tree_edit::parse_statetree_ron(GUARD).expect("fixture parses");
    let text = state_tree_edit::to_canonical_ron(&def).expect("canonical");
    std::fs::write(root.join(FILE), text).expect("write");
    std::fs::write(root.join("ai/sub.statetree.ron"), SUB).expect("write");
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    let mut editor = StEditor::default();
    editor.open(&root.join(FILE)).expect("opens");
    world.insert_resource(editor);
    (root, world)
}

/// What `ai.statetree_edit { path, ops }` writes for `ops` on the fixture file: the same parse, ops and
/// canonical text, and the same refusal (`invalid_tree` with the issues) for a result with errors.
fn agent_bytes(root: &Path, ops: &[StOp]) -> Result<Vec<u8>, Vec<String>> {
    let text = std::fs::read_to_string(root.join(FILE)).expect("reads");
    let mut def = state_tree_edit::parse_statetree_ron(&text).expect("parses");
    state_tree_edit::apply_ops(&mut def, ops).expect("the agent ops apply");
    let load = |include: &str| std::fs::read_to_string(root.join(include)).ok();
    let issues = state_tree_edit::validate(
        &def,
        ValidateOptions {
            registry: None,
            load: Some(&load),
            self_path: Some(FILE),
        },
    );
    if !state_tree_edit::is_valid(&issues) {
        return Err(issues.iter().map(|i| i.code.to_string()).collect());
    }
    Ok(state_tree_edit::to_canonical_ron(&def)
        .expect("canonical")
        .into_bytes())
}

fn raw(events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1600.0, 2400.0),
        )),
        events,
        ..egui::RawInput::default()
    }
}

fn press(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
}

/// The panel alone on a tall screen, clicked by the text of its widgets.
struct Tab {
    ctx: egui::Context,
    world: World,
    root: PathBuf,
    texts: Vec<(String, egui::Pos2)>,
}

impl Tab {
    fn open(label: &str) -> Self {
        let (root, world) = project(label);
        let mut tab = Self {
            ctx: egui::Context::default(),
            world,
            root,
            texts: Vec::new(),
        };
        tab.frame(Vec::new());
        tab
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        let Self {
            ctx, world, texts, ..
        } = self;
        let mut output = ctx.run_ui(raw(events), |ui| panel::state_tree_editor_panel(ui, world));
        output.textures_delta.clear();
        texts.clear();
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                egui::Shape::Text(text) => out.push((
                    text.galley.text().to_string(),
                    shape.visual_bounding_rect().center(),
                )),
                _ => {}
            }
        }
        for clipped in &output.shapes {
            walk(&clipped.shape, texts);
        }
    }

    fn has(&self, label: &str) -> bool {
        self.texts.iter().any(|(t, _)| t == label)
    }

    fn click_nth(&mut self, label: &str, n: usize) {
        self.frame(Vec::new());
        let pos = self
            .texts
            .iter()
            .filter(|(t, _)| t == label)
            .nth(n)
            .map(|(_, p)| *p)
            .unwrap_or_else(|| panic!("no {label:?} #{n} in {:?}", self.texts));
        self.frame(vec![egui::Event::PointerMoved(pos)]);
        self.frame(vec![press(pos, true)]);
        self.frame(vec![press(pos, false)]);
        self.frame(Vec::new());
    }

    fn click(&mut self, label: &str) {
        self.click_nth(label, 0);
    }

    fn editor(&mut self) -> Mut<'_, StEditor> {
        self.world.resource_mut::<StEditor>()
    }

    fn select(&mut self, state: &str) {
        self.editor().selected = Some(state.to_string());
        self.frame(Vec::new());
    }

    fn depth(&self) -> usize {
        self.world.resource::<UndoStack>().undo_depth()
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.root.join(FILE)).expect("reads")
    }

    fn codes(&self) -> Vec<String> {
        diagnose(&self.world).iter().map(|i| i.code.to_string()).collect()
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Fail-before for item 2: the panel draws the tree-level, ambient, include and utility controls.
#[test]
fn the_panel_draws_the_tree_authoring_controls() {
    let mut tab = Tab::open("st-authoring-drawn");
    tab.select("Idle");
    for label in [
        "Apply tree fields",
        "Add ambient task",
        "Remove ambient",
        "Add include",
        "Set utility",
        "Clear utility",
    ] {
        assert!(tab.has(label), "no {label:?} in {:?}", tab.texts);
    }
}

/// One panel operation: `setup` fills the form (and the selection), the labels are clicked in order, and
/// `agent` is the `ai.statetree_edit` ops list for the same change.
struct Case {
    name: &'static str,
    select: Option<&'static str>,
    setup: fn(&mut StEditor),
    labels: &'static [&'static str],
    agent: Vec<StOp>,
}

fn cases() -> Vec<Case> {
    let set_state = |name: &str| StOp::SetState {
        name: name.into(),
        selection: None,
        enter: None,
        weight: None,
        utility: None,
        clear_utility: false,
        fallback: None,
    };
    vec![
        Case {
            name: "add ambient task",
            select: None,
            setup: |e| e.ui.ambient_pick = "ai/sniff".into(),
            labels: &["Add ambient task"],
            agent: vec![StOp::SetAmbient {
                tasks: vec!["ai/listen".into(), "ai/look".into(), "ai/sniff".into()],
            }],
        },
        Case {
            name: "remove ambient task",
            select: None,
            setup: |_| {},
            labels: &["Remove ambient"],
            agent: vec![StOp::SetAmbient {
                tasks: vec!["ai/look".into()],
            }],
        },
        Case {
            name: "tree fields",
            select: None,
            setup: |e| {
                e.ui.tree.description = "guard brain".into();
                e.ui.tree.seed = 7;
                e.ui.tree.state_key = "leaf".into();
            },
            labels: &["Apply tree fields"],
            agent: vec![StOp::SetTree {
                description: Some("guard brain".into()),
                seed: Some(7),
                state_key: Some("leaf".into()),
            }],
        },
        Case {
            name: "set utility",
            select: Some("Alert"),
            setup: |e| {
                e.ui.utility = Some(UtilityDef {
                    key: "threat".into(),
                    scale: 2.0,
                    bias: 0.5,
                });
            },
            labels: &["Set utility"],
            agent: vec![{
                let mut op = set_state("Alert");
                if let StOp::SetState { utility, .. } = &mut op {
                    *utility = Some(UtilityDef {
                        key: "threat".into(),
                        scale: 2.0,
                        bias: 0.5,
                    });
                }
                op
            }],
        },
        Case {
            name: "clear utility",
            select: Some("Idle"),
            setup: |_| {},
            labels: &["Clear utility"],
            agent: vec![{
                let mut op = set_state("Idle");
                if let StOp::SetState { clear_utility, .. } = &mut op {
                    *clear_utility = true;
                }
                op
            }],
        },
        Case {
            name: "add include and list its root",
            select: Some("Root"),
            setup: |e| {
                e.ui.include_prefix = "Combat".into();
                e.ui.include_path = "ai/sub.statetree.ron".into();
            },
            labels: &["Add include", "List under Root"],
            agent: vec![
                StOp::AddInclude {
                    prefix: "Combat".into(),
                    path: "ai/sub.statetree.ron".into(),
                    index: None,
                },
                StOp::ReparentState {
                    name: "Combat/Fight".into(),
                    parent: "Root".into(),
                    index: None,
                },
            ],
        },
        // w10 forms-parity item 2: List under at a chosen index, not only appended.
        Case {
            name: "add include and list its root first",
            select: Some("Root"),
            setup: |e| {
                e.ui.include_prefix = "Combat".into();
                e.ui.include_path = "ai/sub.statetree.ron".into();
                e.ui.include_list_at.insert("Combat".into(), Some(0));
            },
            labels: &["Add include", "List under Root"],
            agent: vec![
                StOp::AddInclude {
                    prefix: "Combat".into(),
                    path: "ai/sub.statetree.ron".into(),
                    index: None,
                },
                StOp::ReparentState {
                    name: "Combat/Fight".into(),
                    parent: "Root".into(),
                    index: Some(0),
                },
            ],
        },
    ]
}

/// w10 forms-parity item 2: the position picker beside **List under** (clicked open, `before Alert`
/// picked) lists the include's root at that index among the parent's children, as one undo step, and
/// saves the bytes of `ai.statetree_edit`'s `reparent_state` with the same `index`.
#[test]
fn list_under_puts_the_included_root_at_the_picked_index() {
    let mut tab = Tab::open("st-authoring-list-at");
    let original = tab.bytes();
    tab.select("Root");
    {
        let mut editor = tab.editor();
        editor.ui.include_prefix = "Combat".into();
        editor.ui.include_path = "ai/sub.statetree.ron".into();
    }
    tab.click("Add include");
    let depth = tab.depth();
    tab.click("at the end");
    tab.click("before Alert");
    assert_eq!(
        tab.world.resource::<StEditor>().ui.include_list_at.get("Combat"),
        Some(&Some(1)),
        "the picker holds the index"
    );
    tab.click("List under Root");
    assert_eq!(tab.depth(), depth + 1, "one undo step");
    let root = tab
        .world
        .resource::<StEditor>()
        .doc
        .states
        .iter()
        .find(|s| s.name == "Root")
        .map(|s| s.children.clone())
        .expect("Root");
    assert_eq!(root, ["Idle", "Combat/Fight", "Alert"]);
    save(&mut tab.world, None).expect("saves");
    let dir = regia_test_utils::unique_temp_dir("st-authoring-list-at-agent");
    std::fs::create_dir_all(dir.join("ai")).expect("dir");
    std::fs::write(dir.join(FILE), &original).expect("write");
    std::fs::write(dir.join("ai/sub.statetree.ron"), SUB).expect("write");
    let agent = agent_bytes(
        &dir,
        &[
            StOp::AddInclude {
                prefix: "Combat".into(),
                path: "ai/sub.statetree.ron".into(),
                index: None,
            },
            StOp::ReparentState {
                name: "Combat/Fight".into(),
                parent: "Root".into(),
                index: Some(1),
            },
        ],
    )
    .unwrap_or_else(|codes| panic!("{codes:?}"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        String::from_utf8_lossy(&tab.bytes()),
        String::from_utf8_lossy(&agent),
        "the panel saves the agent path's bytes"
    );
}

/// Item 2 acceptance: each operation saves byte-identically to the agent path and reloads to the same
/// document; each click is one undo step, and undoing them restores the file's bytes on the next save.
#[test]
fn each_tree_operation_saves_the_agent_bytes_and_undo_restores_them() {
    for case in cases() {
        let mut tab = Tab::open("st-authoring-op");
        let original = tab.bytes();
        let depth = tab.depth();
        if let Some(state) = case.select {
            tab.select(state);
        }
        (case.setup)(&mut tab.editor());
        for label in case.labels {
            tab.click(label);
        }
        assert_eq!(
            tab.depth(),
            depth + case.labels.len(),
            "{}: one undo step per click ({})",
            case.name,
            tab.world.resource::<StEditor>().toast
        );
        save(&mut tab.world, None).unwrap_or_else(|e| panic!("{}: save: {e:?}", case.name));
        let agent = {
            // The agent edits the original file.
            let dir = regia_test_utils::unique_temp_dir("st-authoring-agent");
            std::fs::create_dir_all(dir.join("ai")).expect("dir");
            std::fs::write(dir.join(FILE), &original).expect("write");
            std::fs::write(dir.join("ai/sub.statetree.ron"), SUB).expect("write");
            let bytes =
                agent_bytes(&dir, &case.agent).unwrap_or_else(|codes| panic!("{}: {codes:?}", case.name));
            let _ = std::fs::remove_dir_all(&dir);
            bytes
        };
        let saved = tab.bytes();
        assert_ne!(saved, original, "{}: the save wrote the change", case.name);
        assert_eq!(
            String::from_utf8_lossy(&saved),
            String::from_utf8_lossy(&agent),
            "{}: the panel saves the agent path's bytes",
            case.name
        );
        // Reload: the file opens to the saved document and saves again unchanged.
        let doc = tab.world.resource::<StEditor>().doc.clone();
        let file = tab.root.join(FILE);
        tab.editor().open(&file).expect("reopens");
        assert_eq!(tab.world.resource::<StEditor>().doc, doc, "{}: reload", case.name);
        save(&mut tab.world, None).expect("saves");
        assert_eq!(tab.bytes(), saved, "{}: saving the reloaded tree", case.name);
        // Undo (the reopen started a new session: undo in a fresh one).
        let mut tab = Tab::open("st-authoring-undo");
        let original = tab.bytes();
        if let Some(state) = case.select {
            tab.select(state);
        }
        (case.setup)(&mut tab.editor());
        for label in case.labels {
            tab.click(label);
        }
        for _ in case.labels {
            tab.click("Undo");
        }
        save(&mut tab.world, None).unwrap_or_else(|e| panic!("{}: save after undo: {e:?}", case.name));
        assert_eq!(tab.bytes(), original, "{}: undo restores the bytes", case.name);
    }
}

/// Item 2: an include of the tree itself is an `include_cycle`; the save is refused with `invalid_tree` (as
/// `ai.statetree_edit` refuses it) and writes nothing; undo restores a savable tree with the same bytes.
#[test]
fn an_include_cycle_is_refused_with_the_documented_code() {
    let mut tab = Tab::open("st-authoring-cycle");
    let original = tab.bytes();
    {
        let mut editor = tab.editor();
        editor.ui.include_prefix = "Me".into();
        editor.ui.include_path = FILE.into();
    }
    tab.click("Add include");
    assert!(
        tab.codes().iter().any(|c| c == "include_cycle"),
        "{:?}",
        tab.codes()
    );
    let err = save(&mut tab.world, None).expect_err("refused");
    assert_eq!(err.code, "invalid_tree");
    assert_eq!(tab.bytes(), original, "nothing written");
    let agent = agent_bytes(
        &tab.root,
        &[StOp::AddInclude {
            prefix: "Me".into(),
            path: FILE.into(),
            index: None,
        }],
    )
    .expect_err("the agent path refuses it too");
    assert!(agent.iter().any(|c| c == "include_cycle"), "{agent:?}");
    tab.click("Undo");
    assert!(!tab.codes().iter().any(|c| c == "include_cycle"));
    save(&mut tab.world, None).expect("saves");
    assert_eq!(tab.bytes(), original, "undo restores the bytes");
}

/// Changing and removing an include from its row: the prefix change is carried into the listed root, and
/// removing it drops the listed child; both match the agent path.
#[test]
fn an_include_is_changed_and_removed_from_its_row() {
    let mut tab = Tab::open("st-authoring-include-row");
    tab.select("Root");
    {
        let mut editor = tab.editor();
        editor.ui.include_prefix = "Combat".into();
        editor.ui.include_path = "ai/sub.statetree.ron".into();
    }
    tab.click("Add include");
    tab.click("List under Root");
    assert!(tab.has("listed under Root"), "{:?}", tab.texts);
    save(&mut tab.world, None).expect("saves");
    let listed = tab.bytes();
    tab.editor()
        .ui
        .include_edits
        .insert("Combat".into(), ("Fight".into(), "ai/sub.statetree.ron".into()));
    tab.click("Change include");
    save(&mut tab.world, None).expect("saves after the prefix change");
    let renamed = tab.bytes();
    let rename = StOp::SetInclude {
        prefix: "Combat".into(),
        to_prefix: Some("Fight".into()),
        path: None,
    };
    std::fs::write(tab.root.join(FILE), &listed).expect("write");
    assert_eq!(
        agent_bytes(&tab.root, std::slice::from_ref(&rename)).expect("agent"),
        renamed
    );
    assert!(
        tab.world.resource::<StEditor>().doc.states[0]
            .children
            .contains(&"Fight/Fight".to_string())
    );
    tab.click("Remove include");
    save(&mut tab.world, None).expect("saves after the removal");
    let removed_bytes = tab.bytes();
    std::fs::write(tab.root.join(FILE), &renamed).expect("write");
    let removed = StOp::RemoveInclude {
        prefix: "Fight".into(),
    };
    assert_eq!(agent_bytes(&tab.root, &[removed]).expect("agent"), removed_bytes);
    assert!(tab.world.resource::<StEditor>().doc.includes.is_empty());
}

/// The op builders are the agent's ops (the JSON an agent sends parses to the same op).
#[test]
fn the_op_builders_are_the_agent_ops() {
    let def = state_tree_edit::parse_statetree_ron(GUARD).expect("parses");
    let op = |value: serde_json::Value| -> StOp { serde_json::from_value(value).expect("an agent op") };
    assert_eq!(
        panel::ambient_add_op(&def, "ai/x"),
        op(json!({ "op": "set_ambient", "tasks": ["ai/listen", "ai/look", "ai/x"] }))
    );
    assert_eq!(
        panel::ambient_remove_op(&def, 1),
        op(json!({ "op": "set_ambient", "tasks": ["ai/listen"] }))
    );
    assert_eq!(
        panel::TreeForm::of(&def).op(&def),
        None,
        "an unchanged form edits nothing"
    );
    assert_eq!(panel::include_set_op("A", "p", "A", "p"), None);
    assert_eq!(
        panel::include_set_op("A", "p", "B", "p"),
        Some(op(
            json!({ "op": "set_include", "prefix": "A", "to_prefix": "B" })
        ))
    );
    assert_eq!(
        panel::utility_op("Idle", None),
        op(json!({ "op": "set_state", "name": "Idle", "clear_utility": true }))
    );
    assert_eq!(
        panel::utility_op(
            "Idle",
            Some(UtilityDef {
                key: "k".into(),
                scale: 2.0,
                bias: 0.5
            })
        ),
        op(
            json!({ "op": "set_state", "name": "Idle", "utility": { "key": "k", "scale": 2.0, "bias": 0.5 } })
        )
    );
    let empty = StateTreeDef::new("");
    assert_eq!(
        panel::ambient_remove_op(&empty, 3),
        op(json!({ "op": "set_ambient", "tasks": [] }))
    );
}
