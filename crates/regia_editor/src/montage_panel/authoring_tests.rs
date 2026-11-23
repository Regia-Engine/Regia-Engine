// SPDX-License-Identifier: MPL-2.0
//! Montage authoring from the panel (spec 34 M10): add warp, set blend, set and clear follow, link a section
//! to another or on to the next, add a segment on a second slot track, and the slot-table editor. Each
//! operation is driven by clicking the drawn control and must write the same bytes as the agent method
//! (`anim.montage_edit`, `anim.montage_slot_edit`) for the same change, as one undo step.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;

const COMBO: &str = r#"(format_version: 2, name: "combo", tracks: [(slot: "full", segments: [
    (clip: "a", end: Some(1.0)), (clip: "b", end: Some(1.0))])],
    sections: [(name: "windup", start: 0.0, next: Stop), (name: "swing", start: 1.0,
        follow: Some((track: 0, segment: 1, mode: ShiftWithStart)))],
    notifies: [(name: "hit", time: 1.25)],
    warps: [(target: "enemy", start: 0.2, end: 0.8)])"#;

const ASSET: &str = "anim/combo.montage.ron";
const SLOTS: &str = "anim/hero.slots.ron";

fn project(label: &str) -> (PathBuf, World) {
    let root = regia_test_utils::unique_temp_dir(label);
    std::fs::create_dir_all(root.join("anim")).expect("dir");
    std::fs::write(root.join(ASSET), COMBO).expect("write");
    let mut table = regia_animation::SkeletonSlots::default();
    table.slots[0].name = "full".into();
    std::fs::write(root.join(SLOTS), table.to_ron_string().expect("slots text")).expect("write");
    let mut world = World::new();
    let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root.clone(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: None,
    })
    .expect("opens");
    world.insert_resource(server);
    (root, world)
}

fn bytes(root: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(root.join(rel)).expect("reads")
}

fn raw(events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1600.0, 1400.0),
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

/// The Montage panel alone on a screen, clicked by the text of its widgets.
struct Tab {
    ctx: egui::Context,
    world: World,
    root: PathBuf,
    texts: Vec<(String, egui::Pos2)>,
}

impl Tab {
    fn open(label: &str) -> Self {
        let (root, mut world) = project(label);
        let mut panel = MontagePanel::default();
        panel.open(&mut world, ASSET).expect("opens");
        world.insert_resource(panel);
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
        let mut output = ctx.run_ui(raw(events), |ui| montage_panel(ui, world));
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

    /// Clicks the `n`th widget whose text is `label` (after a frame that lays out the current state).
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

    fn panel(&mut self) -> Mut<'_, MontagePanel> {
        self.world.resource_mut::<MontagePanel>()
    }

    fn error(&self) -> Option<String> {
        self.world
            .resource::<MontagePanel>()
            .error
            .as_ref()
            .map(|e| e.code.clone())
    }

    fn undo_depth(&self) -> u64 {
        self.world
            .resource::<MontagePanel>()
            .last_edit
            .as_ref()
            .map_or(0, |r| r["undo_depth"].as_u64().unwrap_or(0))
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Fail-before for item 1: the authoring controls are drawn (on the base the panel draws none of them).
#[test]
fn the_panel_draws_the_authoring_controls() {
    let mut tab = Tab::open("montage-authoring-drawn");
    tab.world.resource_mut::<MontagePanel>().picked = Some(Picked::Section("swing".into()));
    tab.frame(Vec::new());
    for label in [
        "Add warp",
        "Apply blend",
        "Jump to",
        "Continue",
        "Set follow",
        "Clear follow",
        "Slot table",
        "Add to full",
    ] {
        assert!(tab.has(label), "no {label:?} in {:?}", tab.texts);
    }
    tab.click("Slot table");
    assert!(tab.has("Load slots"), "{:?}", tab.texts);
}

/// One panel operation: `setup` fills the form, `label` is clicked, and `agent` is the `anim.montage_edit`
/// edit an agent sends for the same change.
struct Case {
    name: &'static str,
    setup: fn(&mut MontagePanel),
    label: &'static str,
    agent: Value,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "add warp",
            setup: |p| {
                p.warp.target = "door".into();
                p.warp.start = 0.1;
                p.warp.end = 0.6;
                p.warp.vertical = true;
            },
            label: "Add warp",
            agent: json!({ "op": "add_warp", "target": "door", "start": 0.1, "end": 0.6, "vertical": true }),
        },
        Case {
            name: "set blend",
            setup: |p| {
                if let Some((_, form)) = p.blend.as_mut() {
                    form.blend_in = 0.3;
                    form.auto_blend_out = false;
                }
            },
            label: "Apply blend",
            agent: json!({ "op": "set_blend", "blend_in": 0.3, "auto_blend_out": false }),
        },
        Case {
            name: "set follow",
            setup: |p| {
                p.picked = Some(Picked::Notify("hit".into()));
                p.follow = FollowForm {
                    track: 0,
                    segment: 1,
                    mode: FollowModeEdit::ScaleWithLength,
                };
            },
            label: "Set follow",
            agent: json!({ "op": "set_follow", "kind": "notify", "name": "hit",
                "follow": { "track": 0, "segment": 1, "mode": "scale_with_length" } }),
        },
        Case {
            name: "clear follow",
            setup: |p| p.picked = Some(Picked::Section("swing".into())),
            label: "Clear follow",
            agent: json!({ "op": "set_follow", "kind": "section", "name": "swing" }),
        },
        Case {
            name: "link to another section",
            setup: |p| {
                p.picked = Some(Picked::Section("swing".into()));
                p.link_to = "windup".into();
            },
            label: "Jump to",
            agent: json!({ "op": "link_sections", "from": "swing", "to": "windup" }),
        },
        Case {
            name: "link to next",
            setup: |p| p.picked = Some(Picked::Section("windup".into())),
            label: "Continue",
            agent: json!({ "op": "link_sections", "from": "windup" }),
        },
        Case {
            name: "second slot track",
            setup: |p| {
                p.add_clip = "wave".into();
                p.add_slot = "upper".into();
            },
            label: "Add to upper",
            agent: json!({ "op": "add_segment", "slot": "upper", "clip": "wave" }),
        },
        // w11 forms-undo-autosave item 1: the spec 34 M2 warp fields.
        Case {
            name: "add warp: path_scale mode",
            setup: |p| {
                p.warp.target = "door".into();
                p.warp.mode = WarpMode::PathScale;
            },
            label: "Add warp",
            agent: json!({ "op": "add_warp", "target": "door", "start": 0.0, "end": 0.5, "mode": "path_scale" }),
        },
        Case {
            name: "add warp: rotation kind and fraction",
            setup: |p| {
                p.warp.target = "door".into();
                p.warp.rotation = true;
                p.warp.rotation_kind = RotationKind::FaceTarget;
                p.warp.rotation_fraction = 0.5;
            },
            label: "Add warp",
            agent: json!({ "op": "add_warp", "target": "door", "start": 0.0, "end": 0.5, "rotation": true,
                "rotation_kind": "face_target", "rotation_fraction": 0.5 }),
        },
        Case {
            name: "add warp: match_target",
            setup: |p| {
                p.warp.target = "ledge".into();
                p.warp.rotation_kind = RotationKind::MatchTarget;
            },
            label: "Add warp",
            agent: json!({ "op": "add_warp", "target": "ledge", "start": 0.0, "end": 0.5,
                "rotation_kind": "match_target" }),
        },
        Case {
            name: "add warp: bone anchor",
            setup: |p| {
                p.warp.target = "ledge".into();
                p.warp.anchor = WarpAnchor::Bone("hand_r".into());
            },
            label: "Add warp",
            agent: json!({ "op": "add_warp", "target": "ledge", "start": 0.0, "end": 0.5, "anchor_bone": "hand_r" }),
        },
        Case {
            name: "add warp: offset anchor",
            setup: |p| {
                p.warp.target = "ledge".into();
                p.warp.anchor = WarpAnchor::Offset([0.0, 0.25, 0.8]);
            },
            label: "Add warp",
            agent: json!({ "op": "add_warp", "target": "ledge", "start": 0.0, "end": 0.5,
                "anchor_offset": [0.0, 0.25, 0.8] }),
        },
    ]
}

/// Item 1 acceptance: every operation gives the agent method's bytes, is one undo step, and the panel's
/// Undo restores the file.
#[test]
fn each_panel_operation_writes_the_agent_bytes_in_one_undo_step() {
    for case in cases() {
        let mut tab = Tab::open("montage-authoring-panel");
        let (agent_root, mut agent_world) = project("montage-authoring-agent");
        let original = bytes(&tab.root, ASSET);
        let depth = tab.undo_depth();
        (case.setup)(&mut tab.panel());
        tab.click(case.label);
        assert_eq!(tab.error(), None, "{}", case.name);
        regia_agent::montage_edit(&mut agent_world, &json!({ "asset": ASSET, "edit": case.agent }))
            .unwrap_or_else(|e| panic!("{}: agent edit: {e:?}", case.name));
        let panel_bytes = bytes(&tab.root, ASSET);
        assert_ne!(panel_bytes, original, "{}: the panel wrote the change", case.name);
        assert_eq!(
            String::from_utf8_lossy(&panel_bytes),
            String::from_utf8_lossy(&bytes(&agent_root, ASSET)),
            "{}: the panel and the agent write the same bytes",
            case.name
        );
        assert_eq!(tab.undo_depth(), depth + 1, "{}: one undo step", case.name);
        tab.click("Undo");
        assert_eq!(tab.error(), None, "{}", case.name);
        assert_eq!(
            bytes(&tab.root, ASSET),
            original,
            "{}: undo restores the bytes",
            case.name
        );
        let _ = std::fs::remove_dir_all(&agent_root);
    }
}

/// rev-claude-64 (4) finding 1: the follow form starts from the selected item's own link. Select section
/// `swing` (it follows track 0 segment 1, shift with start), change only the mode and press Set follow: the
/// edit keeps segment 1 (on the base the form kept its default, segment 0, and silently relinked the
/// section). An item with no link starts from the default form, and Undo shows the restored link.
#[test]
fn the_follow_form_starts_from_the_selected_items_link() {
    let mut tab = Tab::open("montage-follow-seed");
    let (agent_root, mut agent_world) = project("montage-follow-seed-agent");
    panel_select(&mut tab.world, &json!({ "kind": "section", "name": "swing" })).expect("selects");
    tab.frame(Vec::new());
    let swing = FollowForm {
        track: 0,
        segment: 1,
        mode: FollowModeEdit::ShiftWithStart,
    };
    assert_eq!(tab.panel().follow, swing, "the form shows swing's link");
    tab.click("keep time");
    tab.click("Set follow");
    assert_eq!(tab.error(), None);
    regia_agent::montage_edit(
        &mut agent_world,
        &json!({ "asset": ASSET, "edit": { "op": "set_follow", "kind": "section", "name": "swing",
            "follow": { "track": 0, "segment": 1, "mode": "keep_time" } } }),
    )
    .expect("agent edit");
    assert_eq!(
        String::from_utf8_lossy(&bytes(&tab.root, ASSET)),
        String::from_utf8_lossy(&bytes(&agent_root, ASSET)),
        "only the mode changed"
    );
    tab.click("Undo");
    assert_eq!(tab.panel().follow, swing, "Undo shows the restored link");
    panel_select(&mut tab.world, &json!({ "kind": "notify", "name": "hit" })).expect("selects");
    tab.frame(Vec::new());
    assert_eq!(tab.panel().follow, FollowForm::default(), "hit follows nothing");
    let _ = std::fs::remove_dir_all(&agent_root);
}

/// The second slot track shows as its own lane with the clip, and a refused warp (end before start) writes
/// nothing and names the agent's code.
#[test]
fn a_second_slot_track_is_a_lane_and_refusals_carry_the_agent_code() {
    let mut tab = Tab::open("montage-authoring-lane");
    {
        let mut p = tab.panel();
        p.add_clip = "wave".into();
        p.add_slot = "upper".into();
    }
    tab.click("Add to upper");
    let described = tab
        .world
        .resource::<MontagePanel>()
        .described
        .clone()
        .expect("described");
    assert_eq!(described["tracks"][1]["slot"], "upper");
    assert_eq!(described["tracks"][1]["segments"][0]["clip"], "wave");
    assert!(tab.has("upper"), "the lane label is drawn");
    // An empty slot field adds to the first track again.
    tab.panel().add_slot.clear();
    tab.click("Add to full");
    let described = tab
        .world
        .resource::<MontagePanel>()
        .described
        .clone()
        .expect("described");
    assert_eq!(described["tracks"][0]["segments"][2]["clip"], "wave");
    let before = bytes(&tab.root, ASSET);
    {
        let mut p = tab.panel();
        p.warp.target = "door".into();
        p.warp.start = 0.9;
        p.warp.end = 0.2;
    }
    tab.click("Add warp");
    let err = regia_agent::montage_edit(
        &mut tab.world,
        &json!({ "asset": ASSET, "edit": { "op": "add_warp", "target": "door", "start": 0.9, "end": 0.2 } }),
    )
    .expect_err("the agent refuses it too");
    let code = err
        .data
        .as_ref()
        .and_then(|d| d["reason"].as_str().or(d["error_code"].as_str()))
        .map(str::to_owned);
    assert_eq!(tab.error(), code, "the panel shows the agent's code");
    assert_eq!(bytes(&tab.root, ASSET), before, "nothing written");
}

/// Item 1 slot table: add, rename and delete from the panel write the agent's bytes (the rename rewrites the
/// open montage too), deleting a slot in use is refused with `slot_in_use`, and add + rename + delete back
/// round-trip the table to its original bytes.
#[test]
fn the_slot_table_editor_round_trips_through_the_agent_edits() {
    let mut tab = Tab::open("montage-slots-panel");
    let (agent_root, mut agent_world) = project("montage-slots-agent");
    let original = bytes(&tab.root, SLOTS);
    let agent = |world: &mut World, op: Value, force: bool| {
        regia_agent::montage_slot_edit(world, &json!({ "path": SLOTS, "op": op, "force": force }))
    };
    tab.click("Slot table");
    tab.panel().slots.path_input = SLOTS.into();
    tab.click("Load slots");
    assert_eq!(tab.error(), None);
    assert!(tab.has("full [default] full body · 1 user(s)"), "{:?}", tab.texts);
    // Add.
    {
        let mut p = tab.panel();
        p.slots.new_name = "upper".into();
        p.slots.new_group = "upper_body".into();
    }
    tab.click("Add slot");
    assert_eq!(tab.error(), None);
    agent(
        &mut agent_world,
        json!({ "op": "add", "name": "upper", "group": "upper_body" }),
        false,
    )
    .expect("agent add");
    assert_eq!(bytes(&tab.root, SLOTS), bytes(&agent_root, SLOTS), "add");
    // Rename `full` (named by the open montage): both files match the agent's, and the lanes show it.
    tab.panel().slots.rename.insert("full".into(), "body".into());
    tab.click_nth("Rename slot", 0);
    assert_eq!(tab.error(), None);
    agent(
        &mut agent_world,
        json!({ "op": "rename", "from": "full", "to": "body" }),
        false,
    )
    .expect("agent rename");
    assert_eq!(
        bytes(&tab.root, SLOTS),
        bytes(&agent_root, SLOTS),
        "rename: table"
    );
    assert_eq!(
        bytes(&tab.root, ASSET),
        bytes(&agent_root, ASSET),
        "rename: montage"
    );
    let described = tab
        .world
        .resource::<MontagePanel>()
        .described
        .clone()
        .expect("described");
    assert_eq!(
        described["tracks"][0]["slot"], "body",
        "the open montage was read again"
    );
    // Delete a slot in use: refused with the agent's code, nothing written.
    let before = bytes(&tab.root, SLOTS);
    tab.click_nth("Delete slot", 0);
    assert_eq!(tab.error().as_deref(), Some("slot_in_use"));
    assert_eq!(bytes(&tab.root, SLOTS), before);
    // Delete `upper`, rename back: the table is byte-identical to the original.
    tab.click_nth("Delete slot", 1);
    assert_eq!(tab.error(), None);
    agent(
        &mut agent_world,
        json!({ "op": "delete", "name": "upper" }),
        false,
    )
    .expect("agent delete");
    assert_eq!(bytes(&tab.root, SLOTS), bytes(&agent_root, SLOTS), "delete");
    tab.panel().slots.rename.insert("body".into(), "full".into());
    tab.click_nth("Rename slot", 0);
    assert_eq!(tab.error(), None);
    assert_eq!(
        bytes(&tab.root, SLOTS),
        original,
        "add, rename, delete round-trip"
    );
    let _ = std::fs::remove_dir_all(&agent_root);
}

/// w10 forms-parity item 1: a slot row's `set` (group and joint mask, then back to full body) from the
/// panel writes the bytes `anim.montage_slot_edit`'s `set` op writes, and the row shows the new mask.
#[test]
fn the_slot_table_sets_a_group_and_joint_mask_like_the_agent() {
    let mut tab = Tab::open("montage-slots-set-panel");
    let (agent_root, mut agent_world) = project("montage-slots-set-agent");
    let agent = |world: &mut World, op: Value| {
        regia_agent::montage_slot_edit(world, &json!({ "path": SLOTS, "op": op }))
    };
    tab.click("Slot table");
    tab.panel().slots.path_input = SLOTS.into();
    tab.click("Load slots");
    assert_eq!(tab.error(), None);
    // The rows are new this frame: draw again so every row widget is painted.
    tab.frame(Vec::new());
    assert!(tab.has("Set slot"), "{:?}", tab.texts);
    // Group and a two-joint mask without children.
    {
        let mut p = tab.panel();
        let form = p
            .slots
            .set_forms
            .get_mut("full")
            .expect("the row's form is seeded");
        assert_eq!(form.group, "default", "seeded from the table");
        form.group = "upper_body".into();
        form.joints = "spine, neck".into();
        form.include_children = false;
    }
    tab.click("Set slot");
    assert_eq!(tab.error(), None);
    agent(
        &mut agent_world,
        json!({ "op": "set", "name": "full", "group": "upper_body", "joints": ["spine", "neck"], "include_children": false }),
    )
    .expect("agent set");
    let masked = bytes(&tab.root, SLOTS);
    assert_eq!(masked, bytes(&agent_root, SLOTS), "set: group and mask");
    assert!(tab.has("full [upper_body] masked · 1 user(s)"), "{:?}", tab.texts);
    // The reloaded form shows the stored mask; clearing the joints makes the slot full body again.
    {
        let mut p = tab.panel();
        let form = p.slots.set_forms.get_mut("full").expect("form");
        assert_eq!(form.joints, "spine, neck", "the form reads the stored mask back");
        assert!(!form.include_children);
        form.joints.clear();
    }
    tab.click("Set slot");
    assert_eq!(tab.error(), None);
    agent(
        &mut agent_world,
        json!({ "op": "set", "name": "full", "full_body": true }),
    )
    .expect("agent full body");
    assert_eq!(
        bytes(&tab.root, SLOTS),
        bytes(&agent_root, SLOTS),
        "set: full body"
    );
    assert!(
        tab.has("full [upper_body] full body · 1 user(s)"),
        "{:?}",
        tab.texts
    );
    let _ = std::fs::remove_dir_all(&agent_root);
}

/// The pure edit builders are the agent's edit objects.
#[test]
fn the_edit_builders_are_the_agent_edit_objects() {
    assert_eq!(
        link_edit("a", Some("b"), false),
        json!({ "op": "link_sections", "from": "a", "to": "b" })
    );
    assert_eq!(
        link_edit("a", None, true),
        json!({ "op": "link_sections", "from": "a", "stop": true })
    );
    assert_eq!(
        link_edit("a", None, false),
        json!({ "op": "link_sections", "from": "a" })
    );
    assert_eq!(follow_edit("notify", "n", None)["follow"], Value::Null);
    let described = json!({ "blend": { "blend_in": 0.25, "blend_out": 0.25, "blend_out_lead": 0.0, "auto_blend_out": true } });
    let form = BlendForm::from_described(&described);
    assert_eq!(form.edit(&described), None, "an unchanged form edits nothing");
    assert_eq!(slot_add_op("s", " "), json!({ "op": "add", "name": "s" }));
    assert_eq!(slot_delete_op("s"), json!({ "op": "delete", "name": "s" }));
    assert_eq!(
        slot_set_op("s", " ", " , ", true),
        json!({ "op": "set", "name": "s", "full_body": true })
    );
    assert_eq!(
        slot_set_op("s", "arms", "l_arm,r_arm ", true),
        json!({ "op": "set", "name": "s", "group": "arms", "joints": ["l_arm", "r_arm"], "include_children": true })
    );
    assert_eq!(
        slot_rename_op("a", "b"),
        json!({ "op": "rename", "from": "a", "to": "b" })
    );
    for edit in [
        WarpForm::default().edit(),
        link_edit("a", Some("b"), false),
        follow_edit("section", "a", Some(&FollowForm::default())),
        add_segment_edit("s", "c"),
    ] {
        serde_json::from_value::<regia_animation::montage_edit::MontageEdit>(edit.clone())
            .unwrap_or_else(|e| panic!("{edit}: {e}"));
    }
}

// ---------------------------------------------------------- w11 forms-undo-autosave item 1

/// The warp form draws its spec 34 M2 controls (mode, rotation kind, fraction, anchor), each written
/// field lands in the file (the M2 cases above compare the bytes with the agent's), and a bone anchor
/// with no bone name sends nothing.
#[test]
fn the_warp_form_draws_and_writes_the_m2_fields() {
    let mut tab = Tab::open("montage-warp-m2");
    for label in ["similarity", "auto", "anchor root"] {
        assert!(tab.has(label), "no {label:?} in {:?}", tab.texts);
    }
    {
        let mut p = tab.panel();
        p.warp.target = "ledge".into();
        p.warp.mode = WarpMode::PathScale;
        p.warp.rotation_kind = RotationKind::MatchTarget;
        p.warp.rotation_fraction = 0.25;
        p.warp.anchor = WarpAnchor::Bone("hand_r".into());
    }
    tab.frame(Vec::new());
    for label in ["path_scale", "match_target", "anchor bone"] {
        assert!(tab.has(label), "no {label:?} in {:?}", tab.texts);
    }
    tab.click("Add warp");
    assert_eq!(tab.error(), None);
    let text = String::from_utf8(bytes(&tab.root, ASSET)).expect("utf8");
    let montage = regia_animation::Montage::from_ron_str(&text).expect("parses");
    let warp = montage.warps.last().expect("a warp");
    assert_eq!(warp.mode, WarpMode::PathScale);
    assert_eq!(warp.rotation_kind, RotationKind::MatchTarget);
    assert_eq!(warp.rotation_fraction, 0.25);
    assert_eq!(warp.anchor, WarpAnchor::Bone("hand_r".into()));
    // A bone anchor needs its bone: Add warp sends nothing without one.
    let before = bytes(&tab.root, ASSET);
    tab.panel().warp.anchor = WarpAnchor::Bone("  ".into());
    tab.click("Add warp");
    assert_eq!(bytes(&tab.root, ASSET), before);
}

/// Item 1 acceptance (slot undo): add, rename (which rewrites the open montage) and delete from the slot
/// table are each one step of the panel's undo journal; Undo three times returns the table and the
/// montage to their original bytes, and the agent route `editor.montage_panel_slot_edit` records the
/// same steps.
#[test]
fn slot_table_edits_undo_to_the_original_bytes() {
    let mut tab = Tab::open("montage-slots-undo");
    let table = bytes(&tab.root, SLOTS);
    let montage = bytes(&tab.root, ASSET);
    tab.click("Slot table");
    tab.panel().slots.path_input = SLOTS.into();
    tab.click("Load slots");
    {
        let mut p = tab.panel();
        p.slots.new_name = "upper".into();
        p.slots.new_group = "upper_body".into();
    }
    tab.click("Add slot");
    assert_eq!(tab.error(), None);
    tab.panel().slots.rename.insert("full".into(), "body".into());
    tab.click_nth("Rename slot", 0);
    assert_eq!(tab.error(), None);
    assert_ne!(bytes(&tab.root, ASSET), montage, "the rename rewrote the montage");
    tab.click_nth("Delete slot", 1);
    assert_eq!(tab.error(), None);
    assert_eq!(tab.panel().journal_depth(), 3, "three slot steps");
    tab.click("Undo");
    assert_eq!(tab.error(), None);
    tab.click("Undo");
    assert_eq!(tab.error(), None);
    assert_eq!(
        bytes(&tab.root, ASSET),
        montage,
        "undoing the rename restores the montage"
    );
    let described = tab.panel().described.clone().expect("described");
    assert_eq!(
        described["tracks"][0]["slot"], "full",
        "the open montage was read again"
    );
    tab.click("Undo");
    assert_eq!(tab.error(), None);
    assert_eq!(
        bytes(&tab.root, SLOTS),
        table,
        "full undo: the table's original bytes"
    );
    assert_eq!(tab.panel().journal_depth(), 0);

    // The agent route: one step, undone through editor.montage_panel_edit.
    let v = panel_slot_edit(
        &mut tab.world,
        &json!({ "path": SLOTS, "op": { "op": "add", "name": "arms" } }),
    )
    .expect("agent slot edit");
    assert_eq!(v["journal_depth"], 1, "{v}");
    assert_ne!(bytes(&tab.root, SLOTS), table);
    let v = panel_edit(&mut tab.world, &json!({ "edit": { "op": "undo" } })).expect("undo");
    assert_eq!(v["report"]["kind"], "slots", "{v}");
    assert_eq!(bytes(&tab.root, SLOTS), table);
}

/// A slot step and a montage step interleave in journal order; a slot table that did not exist (the
/// engine default) is removed again by Undo.
#[test]
fn the_journal_interleaves_montage_and_slot_steps_and_undo_removes_a_new_table() {
    let mut tab = Tab::open("montage-journal-order");
    let montage = bytes(&tab.root, ASSET);
    const NEW_TABLE: &str = "anim/fresh.slots.ron";
    panel_slot_edit(
        &mut tab.world,
        &json!({ "path": NEW_TABLE, "op": { "op": "add", "name": "upper" } }),
    )
    .expect("a new table");
    assert!(tab.root.join(NEW_TABLE).exists());
    panel_edit(
        &mut tab.world,
        &json!({ "edit": { "op": "add_notify", "name": "step", "time": 0.5 } }),
    )
    .expect("montage edit");
    let v = panel_get(&mut tab.world, &json!({})).expect("get");
    assert_eq!(v["journal_depth"], 2, "{v}");
    // Newest first: the notify, then the table.
    panel_edit(&mut tab.world, &json!({ "edit": { "op": "undo" } })).expect("undo notify");
    assert_eq!(bytes(&tab.root, ASSET), montage);
    assert!(tab.root.join(NEW_TABLE).exists(), "the table step is still there");
    let v = panel_edit(&mut tab.world, &json!({ "edit": { "op": "undo" } })).expect("undo table");
    assert_eq!(v["report"]["kind"], "slots");
    assert!(
        !tab.root.join(NEW_TABLE).exists(),
        "Undo removes the table it created"
    );
}

/// Item 1 (New montage): Content ▸ New ▸ Montage writes a `.montage.ron` at the current version that the
/// runtime reader loads and validates, and the Montage timeline opens it.
#[test]
fn a_new_montage_loads_in_the_runtime_and_opens_in_the_panel() {
    use crate::content::{NewAssetKind, create_asset};
    let (root, mut world) = project("montage-new-asset");
    let kind = NewAssetKind::parse("montage").expect("kind");
    assert!(NewAssetKind::ALL.contains(&kind));
    assert_eq!((kind.as_str(), kind.label()), ("montage", "Montage"));
    assert_eq!(kind.extension(), Some("montage.ron"));
    let path = "assets/anim/vault.montage.ron";
    assert_eq!(create_asset(&root, kind, path).expect("created"), path);
    assert!(create_asset(&root, kind, "assets/anim/vault.ron").is_err());
    let text = std::fs::read_to_string(root.join(path)).expect("file");
    let montage = regia_animation::Montage::from_ron_str(&text).expect("the runtime reader loads it");
    montage.validate().expect("valid");
    assert_eq!(montage.format_version, regia_animation::MONTAGE_FORMAT_VERSION);
    assert_eq!(montage.name, "vault");
    assert_eq!(montage.tracks.len(), 1);
    assert_eq!(montage.tracks[0].slot, "default");
    assert_eq!(montage.tracks[0].segments[0].clip, "idle");
    let mut player = regia_animation::MontagePlayer::default();
    player.add_montage(montage);
    let mut panel = MontagePanel::default();
    panel.open(&mut world, path).expect("the timeline opens it");
    assert_eq!(panel.described.as_ref().expect("described")["name"], "vault");
    let _ = std::fs::remove_dir_all(&root);
}

/// The agent surface by name: `editor.montage_panel_slot_edit` is registered with its schema, applies one
/// slot op as one journal step (the same bytes as `anim.montage_slot_edit`), and refuses an op that is not
/// an object with `montage_bad_edit`.
#[test]
fn montage_panel_slot_edit_is_registered_and_dispatches() {
    let mut tab = Tab::open("montage-slot-edit-method");
    let (agent_root, mut agent_world) = project("montage-slot-edit-method-agent");
    let (schema, handler) = methods()
        .into_iter()
        .find(|(m, _)| m.name == "editor.montage_panel_slot_edit")
        .expect("registered");
    assert_eq!(schema.tool, "montage_panel_slot_edit");
    assert!(schema.mutating);
    let names: Vec<&str> = schema.params.iter().map(|p| p.name).collect();
    assert_eq!(names, ["path", "op", "force"]);
    let v = handler(
        &mut tab.world,
        &json!({ "path": SLOTS, "op": { "op": "add", "name": "upper", "group": "upper_body" } }),
    )
    .expect("slot edit");
    assert_eq!(v["journal_depth"], 1, "{v}");
    assert_eq!(v["slots"]["path"], SLOTS, "{v}");
    regia_agent::montage_slot_edit(
        &mut agent_world,
        &json!({ "path": SLOTS, "op": { "op": "add", "name": "upper", "group": "upper_body" } }),
    )
    .expect("agent");
    assert_eq!(bytes(&tab.root, SLOTS), bytes(&agent_root, SLOTS));
    let e = handler(&mut tab.world, &json!({ "op": "add" })).expect_err("op must be an object");
    assert_eq!(
        e.data.as_ref().and_then(|d| d["reason"].as_str()),
        Some("montage_bad_edit"),
        "{e:?}"
    );
    let _ = std::fs::remove_dir_all(&agent_root);
}
