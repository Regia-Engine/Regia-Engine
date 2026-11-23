// SPDX-License-Identifier: MPL-2.0
//! Acceptance of the viewport tools and menu items of `docs/specs/19-editor.md` §§ 2–3 (ADR-7204)
//! through the real headless editor with no GPU: the CPU mesh pick, the eased view presets, the
//! pivot modes, the Alt+drag duplicate, cut/paste, and the Outliner lock and visibility toggles.

use super::*;

/// The first `translation` array anywhere in `value`.
fn find_translation(value: &Value) -> Option<[f64; 3]> {
    match value {
        Value::Object(map) => {
            if let Some(t) = map.get("translation").and_then(Value::as_array)
                && t.len() == 3
            {
                return Some([t[0].as_f64()?, t[1].as_f64()?, t[2].as_f64()?]);
            }
            map.values().find_map(find_translation)
        }
        Value::Array(items) => items.iter().find_map(find_translation),
        _ => None,
    }
}

fn translation(client: &mut Client, entity: u64) -> [f64; 3] {
    let got = call(
        client,
        "scene.get_entity",
        json!({ "entity": entity, "components": ["Transform"] }),
    );
    find_translation(&got).unwrap_or_else(|| panic!("no translation in {got}"))
}

fn near(a: [f64; 3], b: [f64; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
}

fn spawn(client: &mut Client, name: &str, at: [f64; 3], parent: Option<u64>) -> u64 {
    let mut params = json!({ "name": name, "translation": at });
    if let Some(parent) = parent {
        params["parent"] = json!(parent);
    }
    call(client, "editor.spawn", params)["entity"]
        .as_u64()
        .expect("entity id")
}

fn names(tree: &Value) -> Vec<String> {
    fn walk(node: &Value, out: &mut Vec<String>) {
        if let Some(name) = node["name"].as_str() {
            out.push(name.to_string());
        }
        for child in node["children"].as_array().into_iter().flatten() {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    for root in tree["roots"].as_array().into_iter().flatten() {
        walk(root, &mut out);
    }
    out
}

/// A thin ring (torus, tube radius 5 cm, ring radius 2 m) faced to the camera: a click on the
/// ring, two metres from the entity's origin, picks it through the CPU mesh ray (the editor has
/// no GPU). The old translation-sphere fallback (radius max(scale)/2 = 0.5 m about the origin)
/// would miss: the ray passes 2 m from the origin. Then the lock and the eye: a locked or hidden
/// entity is not picked, and unlocking or showing it (the eye is an undo step) brings it back.
#[test]
fn the_cpu_pick_hits_a_thin_mesh_far_from_its_origin_and_skips_locked_and_hidden() {
    let fixture = Fixture::new("headless-viewport-cpu-pick");
    let editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);

    let ring = call(
        &mut client,
        "scene.spawn_primitive",
        json!({
            "shape": "torus",
            "params": { "minor_radius": 0.05, "major_radius": 2.0 },
            "transform": { "translation": [0.0, 1.0, 0.0], "euler_deg": [90.0, 0.0, 0.0] },
            "name": "Ring",
        }),
    );
    let ring_id = ring["entity"].as_u64().expect("ring id");
    assert!(
        ring["triangles"].as_u64().unwrap_or(0) >= 64,
        "enough triangles for the cached BVH path: {ring}"
    );
    // The front preset, instantly: the eye at (0, 1, 8) looking along -Z at (0, 1, 0), fov 60°.
    call(
        &mut client,
        "editor.view_preset",
        json!({ "preset": "front", "instant": true }),
    );
    // Row 68 of 240 is 2.0 m above the target at 8 m (tan 30° · 8 m · (1 - 2·68/240)).
    let at = json!({ "x": 160.0, "y": 68.0, "width": 320, "height": 240 });
    let hit = call(&mut client, "editor.pick", at.clone());
    assert_eq!(hit["method"], "cpu", "{hit}");
    assert_eq!(hit["entity"].as_u64(), Some(ring_id), "the ring is picked: {hit}");
    let y = hit["position"][1].as_f64().unwrap_or(0.0);
    assert!(
        (y - 3.0).abs() < 0.1,
        "on the ring's top, 2 m above its origin: {hit}"
    );
    // The center pixel looks through the ring's hole at its origin: background.
    let center = call(
        &mut client,
        "editor.pick",
        json!({ "x": 160.0, "y": 120.0, "width": 320, "height": 240 }),
    );
    assert_eq!(
        center["entity"],
        Value::Null,
        "the hole is not the ring: {center}"
    );

    // Locked: not picked. Unlocked: picked again.
    let locked = call(
        &mut client,
        "editor.outliner_set",
        json!({ "entity": ring_id, "locked": true }),
    );
    assert_eq!(locked["locked"], true, "{locked}");
    let miss = call(&mut client, "editor.pick", at.clone());
    assert_eq!(
        miss["entity"],
        Value::Null,
        "a locked entity is not picked: {miss}"
    );
    call(
        &mut client,
        "editor.outliner_set",
        json!({ "entity": ring_id, "locked": false }),
    );
    assert_eq!(
        call(&mut client, "editor.pick", at.clone())["entity"].as_u64(),
        Some(ring_id)
    );

    // Hidden by the eye: not drawn, so not picked; the eye is one undo step.
    let hidden = call(
        &mut client,
        "editor.outliner_set",
        json!({ "entity": ring_id, "visible": false }),
    );
    assert_eq!(hidden["visible"], false, "{hidden}");
    let miss = call(&mut client, "editor.pick", at.clone());
    assert_eq!(
        miss["entity"],
        Value::Null,
        "a hidden entity is not picked: {miss}"
    );
    call(&mut client, "editor.undo", json!({}));
    assert_eq!(
        call(&mut client, "editor.pick", at)["entity"].as_u64(),
        Some(ring_id),
        "undo shows it again"
    );
}

/// A preset eases over its fixed duration: halfway the camera is between the start and the
/// preset pose, at the end exactly on the preset pose.
#[test]
fn a_view_preset_lands_on_the_exact_pose_after_the_ease() {
    let fixture = Fixture::new("headless-viewport-preset-ease");
    let editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);

    let started = call(&mut client, "editor.view_preset", json!({ "preset": "top" }));
    let duration = started["duration"].as_f64().expect("duration");
    assert!(duration > 0.0, "{started}");
    let from_pitch = started["from"]["pitch"].as_f64().expect("from pitch");
    let to = started["to"].clone();
    let to_pitch = to["pitch"].as_f64().expect("to pitch");
    assert!(to_pitch > from_pitch, "top looks down: {started}");

    let mid = call(
        &mut client,
        "editor.camera_ease",
        json!({ "advance": duration * 0.5 }),
    );
    assert_eq!(mid["active"], true, "{mid}");
    let mid_pitch = mid["camera"]["pitch"].as_f64().expect("mid pitch");
    assert!(
        mid_pitch > from_pitch && mid_pitch < to_pitch,
        "halfway is between: {from_pitch} < {mid_pitch} < {to_pitch}"
    );

    let end = call(
        &mut client,
        "editor.camera_ease",
        json!({ "advance": duration * 0.5 }),
    );
    assert_eq!(end["active"], false, "{end}");
    for key in ["target", "distance", "yaw", "pitch", "eye"] {
        assert_eq!(end["camera"][key], to[key], "{key} lands exactly: {end}");
    }
    // Further time changes nothing.
    let after = call(&mut client, "editor.camera_ease", json!({ "advance": 1.0 }));
    assert_eq!(after["camera"], end["camera"]);
}

/// Two entities at x = 0 and x = 2 turned half a turn about Y: about the median point (1, 0, 0)
/// they swap places; about the active one (x = 2) the other goes to x = 4; about individual
/// origins neither moves. Each drag is one undo step.
#[test]
fn pivot_modes_rotate_a_two_object_selection_about_the_expected_points() {
    let fixture = Fixture::new("headless-viewport-pivot");
    let editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);
    let a = spawn(&mut client, "A", [0.0, 0.0, 0.0], None);
    let b = spawn(&mut client, "B", [2.0, 0.0, 0.0], None);

    let cases = [
        ("median", [2.0, 0.0, 0.0], [0.0, 0.0, 0.0]),
        ("active", [4.0, 0.0, 0.0], [2.0, 0.0, 0.0]),
        ("individual", [0.0, 0.0, 0.0], [2.0, 0.0, 0.0]),
    ];
    for (mode, want_a, want_b) in cases {
        let set = call(&mut client, "editor.set_pivot_mode", json!({ "mode": mode }));
        assert_eq!(set["mode"], mode);
        let before = call(&mut client, "editor.history", json!({}));
        let dragged = call(
            &mut client,
            "editor.gizmo_drag",
            json!({ "entities": [a, b], "active": b, "rotate_deg": [0.0, 180.0, 0.0] }),
        );
        assert_eq!(dragged["pivot_mode"], mode, "{dragged}");
        assert!(
            near(translation(&mut client, a), want_a),
            "{mode}: A at {:?}, want {want_a:?}",
            translation(&mut client, a)
        );
        assert!(
            near(translation(&mut client, b), want_b),
            "{mode}: B at {:?}, want {want_b:?}",
            translation(&mut client, b)
        );
        let after = call(&mut client, "editor.history", json!({}));
        assert_eq!(
            after["undo"].as_array().map(Vec::len),
            before["undo"].as_array().map(|u| u.len() + 1),
            "one undo step: {after}"
        );
        call(&mut client, "editor.undo", json!({}));
        assert!(
            near(translation(&mut client, a), [0.0, 0.0, 0.0]),
            "undo restores A"
        );
        assert!(
            near(translation(&mut client, b), [2.0, 0.0, 0.0]),
            "undo restores B"
        );
    }
    assert_eq!(
        error_code(client.call("editor.set_pivot_mode", json!({ "mode": "corner" }))),
        "F0002"
    );
}

/// Alt+drag: the original stays, a copy moves; one undo removes the copy.
#[test]
fn alt_drag_moves_a_copy_and_one_undo_removes_it() {
    let fixture = Fixture::new("headless-viewport-alt-drag");
    let editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);
    let original = spawn(&mut client, "Pillar", [1.0, 0.0, 0.0], None);
    let count = |client: &mut Client| call(client, "editor.hierarchy", json!({}))["count"].as_u64();
    let before = count(&mut client);

    let dragged = call(
        &mut client,
        "editor.gizmo_drag",
        json!({ "entities": [original], "translate": [0.0, 0.0, 3.0], "duplicate": true }),
    );
    let copies = dragged["copies"].as_array().cloned().unwrap_or_default();
    assert_eq!(copies.len(), 1, "{dragged}");
    let copy = copies[0].as_u64().expect("copy id");
    assert_ne!(copy, original);
    assert!(
        near(translation(&mut client, original), [1.0, 0.0, 0.0]),
        "the original stays"
    );
    assert!(
        near(translation(&mut client, copy), [1.0, 0.0, 3.0]),
        "the copy moved"
    );
    assert_eq!(count(&mut client), before.map(|c| c + 1));

    let undone = call(&mut client, "editor.undo", json!({}));
    assert_eq!(undone["undone"], true, "{undone}");
    assert_eq!(count(&mut client), before, "one undo removes the copy");
    assert!(near(translation(&mut client, original), [1.0, 0.0, 0.0]));
    // Redo brings the moved copy back.
    call(&mut client, "editor.redo", json!({}));
    assert_eq!(count(&mut client), before.map(|c| c + 1));
}

/// Edit ▸ Cut then Edit ▸ Paste round-trips an entity with its child; Edit ▸ Duplicate (Ctrl+D)
/// and Select all go through the same menu path.
#[test]
fn cut_and_paste_round_trip_an_entity_with_children() {
    let fixture = Fixture::new("headless-viewport-cut-paste");
    let editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);
    let parent = spawn(&mut client, "Cart", [0.0, 0.0, 0.0], None);
    spawn(&mut client, "Wheel", [0.5, 0.0, 0.0], Some(parent));
    call(&mut client, "editor.select", json!({ "entities": [parent] }));

    let cut = call(&mut client, "editor.menu_invoke", json!({ "path": "Edit/Cut" }));
    assert_eq!(cut["copied"], 1, "{cut}");
    let tree = call(&mut client, "editor.hierarchy", json!({}));
    assert!(
        !names(&tree).iter().any(|n| n == "Cart" || n == "Wheel"),
        "{tree}"
    );

    let pasted = call(&mut client, "editor.menu_invoke", json!({ "path": "Edit/Paste" }));
    let roots = pasted["pasted"].as_array().cloned().unwrap_or_default();
    assert_eq!(roots.len(), 1, "{pasted}");
    let tree = call(&mut client, "editor.hierarchy", json!({}));
    let cart = tree["roots"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|node| node["name"] == "Cart")
        .cloned()
        .unwrap_or_else(|| panic!("Cart is back: {tree}"));
    assert_eq!(cart["children"][0]["name"], "Wheel", "with its child: {cart}");
    assert_eq!(cart["entity"].as_u64(), roots[0].as_u64());

    // Duplicate is one undo step; Select all selects every entity.
    let duplicated = call(
        &mut client,
        "editor.menu_invoke",
        json!({ "path": "Edit/Duplicate" }),
    );
    assert_eq!(
        duplicated["copies"].as_array().map(Vec::len),
        Some(1),
        "{duplicated}"
    );
    call(&mut client, "editor.undo", json!({}));
    let all = call(
        &mut client,
        "editor.menu_invoke",
        json!({ "path": "Edit/Select all" }),
    );
    let count = call(&mut client, "editor.hierarchy", json!({}))["count"].clone();
    assert_eq!(all["selected"], count, "{all}");
    let empty = client.call("editor.menu_invoke", json!({ "path": "Edit/Frobnicate" }));
    assert_eq!(error_code(empty), "F0002");
}

/// The root entity named `name` in `editor.hierarchy`.
fn root_named(client: &mut Client, name: &str) -> u64 {
    let tree = call(client, "editor.hierarchy", json!({}));
    tree["roots"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|node| node["name"] == name)
        .and_then(|node| node["entity"].as_u64())
        .unwrap_or_else(|| panic!("no root {name} in {tree}"))
}

fn camera_now(client: &mut Client) -> Value {
    call(client, "editor.camera_ease", json!({ "advance": 0.0 }))["camera"].clone()
}

fn same_camera(a: &Value, b: &Value) -> bool {
    ["target", "eye"].iter().all(|k| {
        let (x, y) = (&a[*k], &b[*k]);
        (0..3).all(|i| (x[i].as_f64().unwrap_or(f64::NAN) - y[i].as_f64().unwrap_or(f64::NAN)).abs() < 1e-4)
    }) && ["distance", "yaw", "pitch"]
        .iter()
        .all(|k| (a[*k].as_f64().unwrap_or(f64::NAN) - b[*k].as_f64().unwrap_or(f64::NAN)).abs() < 1e-4)
}

/// post05w4 item 1 (the editor-viewport lane's "Recent lists this session's levels only; the
/// Outliner lock is not saved with the level"): the view state of a level survives an editor
/// restart. The first editor moves the camera to the top preset, stores bookmark 3 at the front
/// preset, locks the Crate and exits; a second editor on the same project lists the level under
/// File ▸ Recent, and opening it brings back the camera, the bookmark and the lock.
#[test]
fn the_view_state_of_a_level_survives_an_editor_restart() {
    let fixture = Fixture::new("headless-viewport-persist");
    let mut first = Editor::start(&fixture);
    first.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);
    call(
        &mut client,
        "editor.view_preset",
        json!({ "preset": "front", "instant": true }),
    );
    let mark = call(
        &mut client,
        "editor.bookmark",
        json!({ "action": "set", "slot": 3 }),
    )["camera"]
        .clone();
    call(
        &mut client,
        "editor.view_preset",
        json!({ "preset": "top", "instant": true }),
    );
    let top = camera_now(&mut client);
    assert!(!same_camera(&top, &mark), "two different poses: {top} {mark}");
    let crate_id = root_named(&mut client, "Crate");
    call(
        &mut client,
        "editor.outliner_set",
        json!({ "entity": crate_id, "locked": true }),
    );
    let out = call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(out["exiting"], true, "{out}");
    first.event("exit", &fixture);
    assert_eq!(first.wait().code(), Some(0), "{}", fixture.log());
    drop(client);

    let mut second = Editor::start(&fixture);
    second.event("ready", &fixture);
    let mut client = connect(&fixture);
    let recent = call(
        &mut client,
        "editor.menu_invoke",
        json!({ "path": "File/Recent" }),
    );
    assert!(
        recent["recent"].as_array().is_some_and(|r| r
            .iter()
            .any(|p| p.as_str().is_some_and(|p| p.ends_with("room.level.ron")))),
        "File ▸ Recent lists the level of the last session: {recent}"
    );
    call(
        &mut client,
        "editor.open_level",
        json!({ "path": "assets/maps/room.level.ron" }),
    );
    let camera = camera_now(&mut client);
    assert!(
        same_camera(&camera, &top),
        "the camera comes back: {camera} vs {top}"
    );
    let marks = call(&mut client, "editor.bookmark", json!({ "action": "list" }));
    let slot3 = marks["bookmarks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["slot"] == 3)
        .map(|m| m["camera"].clone())
        .unwrap_or_else(|| panic!("bookmark 3 comes back: {marks}"));
    assert!(same_camera(&slot3, &mark), "{slot3} vs {mark}");
    let crate_id = root_named(&mut client, "Crate");
    let state = call(&mut client, "editor.outliner_set", json!({ "entity": crate_id }));
    assert_eq!(state["locked"], true, "the lock comes back: {state}");
    call(&mut client, "agent.shutdown", json!({ "save": false }));
    second.event("exit", &fixture);
    assert_eq!(second.wait().code(), Some(0), "{}", fixture.log());
}

/// The viewport selection tools of ADR-7769 over the real agent dispatcher (no GPU): select
/// similar (P-065: on a level with 3 lights and 2 meshes, "same kind" from a light selects the
/// 3 lights and from a mesh the 2 meshes), the pick stack under the cursor (P-073: two cubes on
/// the center ray, nearest first; choosing row 1 selects the far one), the selection root
/// (P-074) and the measure tool (P-069).
#[test]
fn select_similar_pick_stack_selection_root_and_measure_answer_over_the_protocol() {
    let fixture = Fixture::new("headless-viewport-select-place");
    let editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);
    let create = |client: &mut Client, kind: &str, at: [f64; 3]| {
        call(
            client,
            "editor.create_entity",
            json!({ "kind": kind, "translation": at }),
        )["entity"]
            .as_u64()
            .expect("entity")
    };
    let lights: Vec<u64> = [6.0, 7.0, 8.0]
        .into_iter()
        .map(|x| create(&mut client, "point_light", [x, 1.0, 0.0]))
        .collect();
    let near_cube = create(&mut client, "cuboid", [0.0, 1.0, 0.0]);
    let far_cube = create(&mut client, "cuboid", [0.0, 1.0, -3.0]);
    let ids = |value: &Value| -> Vec<u64> {
        let mut out: Vec<u64> = value["entities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["entity"].as_u64())
            .collect();
        out.sort_unstable();
        out
    };

    // Select similar: same kind from a light, then from a mesh.
    call(&mut client, "editor.select", json!({ "entities": [lights[0]] }));
    let similar = call(&mut client, "editor.select_similar", json!({ "by": "kind" }));
    let mut want = lights.clone();
    want.sort_unstable();
    assert_eq!(
        ids(&similar),
        want,
        "same kind from a light: the 3 lights: {similar}"
    );
    call(&mut client, "editor.select", json!({ "entities": [near_cube] }));
    let similar = call(&mut client, "editor.select_similar", json!({ "by": "kind" }));
    let mut cubes = vec![near_cube, far_cube];
    cubes.sort_unstable();
    assert_eq!(
        ids(&similar),
        cubes,
        "same kind from a mesh: the 2 meshes: {similar}"
    );
    call(&mut client, "editor.select", json!({ "entities": [] }));
    assert_eq!(
        error_code_or_reason(client.call("editor.select_similar", json!({ "by": "kind" }))),
        "nothing_selected"
    );

    // The pick stack: the front view, the center ray passes through both cubes.
    call(
        &mut client,
        "editor.view_preset",
        json!({ "preset": "front", "instant": true }),
    );
    let stack = call(
        &mut client,
        "editor.pick_stack",
        json!({ "x": 160.0, "y": 120.0, "width": 320, "height": 240, "cpu": true, "select": 1 }),
    );
    let rows: Vec<u64> = stack["entities"]
        .as_array()
        .expect("rows")
        .iter()
        .filter_map(|e| e["entity"].as_u64())
        .collect();
    assert!(rows.len() >= 2, "{stack}");
    assert_eq!(&rows[..2], &[near_cube, far_cube], "nearest first: {stack}");
    assert_eq!(stack["selected"].as_u64(), Some(far_cube), "{stack}");
    let d0 = stack["entities"][0]["distance"].as_f64().unwrap_or(0.0);
    let d1 = stack["entities"][1]["distance"].as_f64().unwrap_or(0.0);
    assert!(d0 < d1, "{stack}");

    // The selection root: a click on the child selects the marked parent.
    let root = call(
        &mut client,
        "editor.spawn",
        json!({ "name": "Root", "translation": [0.0, 0.0, 0.0] }),
    )["entity"]
        .as_u64()
        .expect("root");
    let child = call(
        &mut client,
        "editor.spawn",
        json!({ "name": "Child", "translation": [0.0, 0.0, 0.0], "parent": root }),
    )["entity"]
        .as_u64()
        .expect("child");
    let before = call(
        &mut client,
        "editor.selection_root",
        json!({ "entity": root, "probe": child }),
    );
    assert_eq!(before["root"], false, "{before}");
    assert_eq!(before["click_selects"].as_u64(), Some(child), "{before}");
    let marked = call(
        &mut client,
        "editor.selection_root",
        json!({ "entity": root, "set": true, "probe": child }),
    );
    assert_eq!(marked["root"], true, "{marked}");
    assert_eq!(marked["click_selects"].as_u64(), Some(root), "{marked}");
    assert_eq!(
        error_code_or_reason(client.call("editor.selection_root", json!({ "entity": 999_999_999u64 }))),
        "no_entity"
    );

    // The measure tool: 3 m, then the right angle at b.
    let m = call(
        &mut client,
        "editor.measure",
        json!({ "a": [0.0, 0.0, 0.0], "b": [3.0, 0.0, 0.0], "c": [3.0, 0.0, 4.0] }),
    );
    let measured = &m["measurement"];
    assert!(
        (measured["distance"].as_f64().unwrap_or(0.0) - 3.0).abs() < 1e-5,
        "{m}"
    );
    assert!(
        (measured["angle_deg"].as_f64().unwrap_or(0.0) - 90.0).abs() < 1e-3,
        "{m}"
    );
    assert_eq!(m["active"], true, "a measured line shows in the viewport: {m}");
    let read = call(&mut client, "editor.measure", json!({}));
    assert_eq!(read["measurement"], m["measurement"], "{read}");
    let left = call(&mut client, "editor.measure", json!({ "active": false }));
    assert_eq!(left["measurement"], Value::Null, "{left}");

    // The drop ghost's query (P-067): a sound would stand on the near cube's face under the
    // center pixel; nothing is spawned until `place`.
    let at = json!({ "path": "assets/boom.wav", "x": 160.0, "y": 120.0, "width": 320, "height": 240 });
    let ghost = call(&mut client, "editor.drop_preview", at.clone());
    assert_eq!(ghost["places"], true, "{ghost}");
    let z = ghost["ghost"]["translation"][2].as_f64().unwrap_or(f64::NAN);
    assert!(
        (z - 0.5).abs() < 0.05,
        "on the near cube's front face (z = 0.5): {ghost}"
    );
    let material = call(
        &mut client,
        "editor.drop_preview",
        json!({ "path": "assets/paint.material.ron" }),
    );
    assert_eq!(
        material["places"], false,
        "a material assigns, it does not place: {material}"
    );
    let mut placed = at;
    placed["place"] = json!(true);
    let dropped = call(&mut client, "editor.drop_preview", placed);
    assert!(
        dropped["placed"]
            .as_str()
            .is_some_and(|t| t.starts_with("Spawned")),
        "{dropped}"
    );
}
