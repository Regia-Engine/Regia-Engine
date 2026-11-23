// SPDX-License-Identifier: MPL-2.0
//! Tests of the shot-list document (w7 items 2 and 3, ADR-8730).

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_framework::shots::ShotPlayer;
use serde_json::{Value, json};

use crate::agent_ext::EditorProject;

const REL: &str = "assets/cine/intro.shots.ron";

/// A project with one new shot list (`editor.create_asset`'s file) at [`REL`].
fn project(label: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(&format!("shot-document-{label}"));
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    crate::content::create_asset(&root, crate::content::NewAssetKind::ShotList, REL).expect("shot list");
    root
}

fn world_in(root: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(EditorProject {
        root: Some(root.to_path_buf()),
    });
    world.insert_resource(ShotPlayer::default());
    world
}

/// Runs one editor agent method through its installed handler (the dispatcher's table).
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params)
}

fn ok(world: &mut World, method: &str, params: Value) -> Value {
    call(world, method, params).unwrap_or_else(|e| panic!("{method}: {}", e.message))
}

fn code(e: regia_agent::protocol::RpcError) -> String {
    e.data
        .and_then(|d| d["code"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Acceptance (2): a cut added in the timeline, saved, reopened, keeps the cut; a second save is
/// byte-identical; `editor.shot_panel_state` reports dirty after the edit and clean after the save.
#[test]
fn a_timeline_cut_saved_and_reopened_keeps_the_cut() {
    let root = project("cut");
    let mut world = world_in(&root);
    let opened = ok(&mut world, "editor.shot_panel_open", json!({ "path": REL }));
    assert_eq!(opened["path"], REL, "{opened}");
    assert_eq!(opened["dirty"], false, "{opened}");
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["path"], REL, "{state}");
    assert_eq!(state["dirty"], false, "{state}");
    assert_eq!(state["shots"].as_array().map(Vec::len), Some(1), "{state}");

    // The timeline's Add cut: the sequence player has no shot list of its own, so the cut row edits
    // the ShotPlayer's list (the document).
    let sequence = regia_framework::sequencer::edit::tracks::new_sequence("intro", 5.0, 30.0).expect("new");
    world.spawn(regia_framework::sequencer::SequencePlayer::new(sequence));
    let camera = world
        .spawn((
            regia_render::Camera::default(),
            regia_scene::Transform {
                translation: regia_core::math::Vec3::new(0.0, 2.0, 6.0),
                ..Default::default()
            },
        ))
        .id();
    let cut = ok(
        &mut world,
        "editor.seq_cut_edit",
        json!({ "op": "add", "time": 2.0, "camera": camera.to_bits() }),
    );
    assert_eq!(cut["cuts"]["host"], "shot_player", "{cut}");
    assert_eq!(cut["cuts"]["cuts"], json!([2.0]), "{cut}");
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["dirty"], true, "an edit makes the document dirty: {state}");
    assert_eq!(state["shots"].as_array().map(Vec::len), Some(2), "{state}");

    let saved = ok(&mut world, "editor.shot_panel_save", json!({}));
    assert_eq!(saved["path"], REL, "{saved}");
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["dirty"], false, "clean after the save: {state}");
    let first = std::fs::read(root.join(REL)).expect("saved");
    ok(&mut world, "editor.shot_panel_save", json!({}));
    assert_eq!(
        std::fs::read(root.join(REL)).expect("saved"),
        first,
        "a second save is byte-identical"
    );

    // A fresh session reopens it: the cut is there.
    let mut again = world_in(&root);
    ok(&mut again, "editor.shot_panel_open", json!({ "path": REL }));
    let list = again
        .resource::<ShotPlayer>()
        .sequence()
        .cloned()
        .expect("loaded");
    assert_eq!(list.shots.len(), 2, "the cut survived the save");
    assert!((list.shot_start(1) - 2.0).abs() < 1e-9, "at 2 s");
    let _ = std::fs::remove_dir_all(&root);
}

/// Acceptance (2): saving to a read-only place answers `shot_panel_io` and the document stays
/// dirty; the refusals of open and save carry their codes and change nothing.
#[test]
fn refused_saves_and_opens_carry_codes_and_keep_the_document() {
    let root = project("refuse");
    let mut world = world_in(&root);
    // Nothing loaded.
    let err = call(&mut world, "editor.shot_panel_save", json!({})).expect_err("nothing loaded");
    assert_eq!(code(err), "shot_panel_no_sequence");
    for (path, expected) in [
        ("assets/cine/intro.seq.ron", "shot_panel_bad_path"),
        ("../outside.shots.ron", "shot_panel_bad_path"),
        ("assets/cine/missing.shots.ron", "shot_panel_io"),
    ] {
        let err = call(&mut world, "editor.shot_panel_open", json!({ "path": path })).expect_err(path);
        assert_eq!(code(err), expected, "{path}");
    }
    std::fs::write(root.join("assets/cine/bad.shots.ron"), "(version: 2, shots: [])").expect("bad");
    let err = call(
        &mut world,
        "editor.shot_panel_open",
        json!({ "path": "assets/cine/bad.shots.ron" }),
    )
    .expect_err("no shots");
    assert_eq!(code(err), "shot_panel_bad_file");

    ok(&mut world, "editor.shot_panel_open", json!({ "path": REL }));
    ok(
        &mut world,
        "editor.shot_panel_edit",
        json!({ "op": "add", "time": 1.0, "value": 50.0 }),
    );
    assert_eq!(
        ok(&mut world, "editor.shot_panel_state", json!({}))["dirty"],
        true
    );
    // Opening over unsaved edits is refused unless `discard` says so.
    let err = call(&mut world, "editor.shot_panel_open", json!({ "path": REL })).expect_err("unsaved");
    assert_eq!(code(err), "shot_panel_unsaved");
    assert_eq!(
        ok(&mut world, "editor.shot_panel_state", json!({}))["dirty"],
        true
    );
    let err = call(
        &mut world,
        "editor.shot_panel_save",
        json!({ "path": "assets/cine/intro.seq.ron" }),
    )
    .expect_err("bad path");
    assert_eq!(code(err), "shot_panel_bad_path");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = root.join("assets/cine");
        let keep = std::fs::metadata(&dir).expect("meta").permissions();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("read-only");
        // Root ignores the mode: then there is no read-only place to test.
        let probe = dir.join("probe.tmp");
        let writable = std::fs::write(&probe, b"x").is_ok();
        let _ = std::fs::remove_file(&probe);
        if !writable {
            let before = std::fs::read(root.join(REL)).expect("file");
            let err = call(&mut world, "editor.shot_panel_save", json!({})).expect_err("read-only");
            assert_eq!(code(err), "shot_panel_io");
            let state = ok(&mut world, "editor.shot_panel_state", json!({}));
            assert_eq!(state["dirty"], true, "a failed save keeps dirty: {state}");
            assert_eq!(
                std::fs::read(root.join(REL)).expect("file"),
                before,
                "file untouched"
            );
        }
        std::fs::set_permissions(&dir, keep).expect("restore");
    }

    let reopened = ok(
        &mut world,
        "editor.shot_panel_open",
        json!({ "path": REL, "discard": true }),
    );
    assert_eq!(reopened["dirty"], false, "{reopened}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A list loaded without a document (`render.sequence`, the timeline's first Add cut) has no
/// path: Save needs one, and saving to one makes it the document's path.
#[test]
fn a_list_without_a_document_saves_to_a_given_path() {
    let root = project("adopt");
    let mut world = world_in(&root);
    let text = std::fs::read_to_string(root.join(REL)).expect("file");
    let list = regia_framework::shots::ShotSequence::from_ron(&text).expect("list");
    world.resource_mut::<ShotPlayer>().load(list);
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["path"], Value::Null, "{state}");
    assert_eq!(state["dirty"], true, "never saved: {state}");
    let err = call(&mut world, "editor.shot_panel_save", json!({})).expect_err("no path");
    assert_eq!(code(err), "shot_panel_bad_path");
    let other = "assets/cine/copy.shots.ron";
    ok(&mut world, "editor.shot_panel_save", json!({ "path": other }));
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["path"], other, "{state}");
    assert_eq!(state["dirty"], false, "{state}");
    assert_eq!(
        std::fs::read_to_string(root.join(other)).expect("copy"),
        text,
        "the same list writes the same text"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// rev-claude-57 seq-shot finding 1 (ADR-7495): after opening A, a list loaded straight into the
/// player (`render.sequence`) or a `clear` and load is not A any more: the document drops A's
/// path, so Save, Ctrl+S and the autosave never write the new list over A's file.
#[test]
fn a_list_loaded_over_an_open_document_drops_its_path() {
    let root = project("reload");
    let mut world = world_in(&root);
    let a_before = std::fs::read(root.join(REL)).expect("A");
    ok(&mut world, "editor.shot_panel_open", json!({ "path": REL }));
    assert_eq!(crate::shot_document::path(&world).as_deref(), Some(REL));
    // B: A plus a second shot (any other valid list will do).
    let mut b = world
        .resource::<ShotPlayer>()
        .sequence()
        .cloned()
        .expect("A is loaded");
    let first = b.shots[0].clone();
    b.shots.push(regia_framework::shots::Shot {
        name: format!("{}_b", first.name),
        ..first
    });
    for reload in [false, true] {
        if reload {
            world.resource_mut::<ShotPlayer>().clear();
        }
        world.resource_mut::<ShotPlayer>().load(b.clone());
        let state = ok(&mut world, "editor.shot_panel_state", json!({}));
        assert_eq!(state["path"], Value::Null, "B is not A's file: {state}");
        assert_eq!(state["dirty"], true, "B was never saved: {state}");
        assert!(
            super::dirty(&world).is_empty(),
            "no autosave key: B has no file to restore to"
        );
        let err = call(&mut world, "editor.shot_panel_save", json!({})).expect_err("no path");
        // ADR-8940 (merged over ADR-7495): a detached list's refusal names the detach.
        assert_eq!(code(err), "shot_panel_detached");
        assert_eq!(std::fs::read(root.join(REL)).expect("A"), a_before, "A untouched");
    }
    // An edit of the loaded list (`replace_sequence`, the timeline's cuts) keeps the document.
    let other = "assets/cine/b.shots.ron";
    ok(&mut world, "editor.shot_panel_save", json!({ "path": other }));
    let mut edited = b.clone();
    edited.shots.pop();
    world.resource_mut::<ShotPlayer>().replace_sequence(edited);
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["path"], other, "an edit keeps the path: {state}");
    assert_eq!(state["dirty"], true, "{state}");
    let _ = std::fs::remove_dir_all(&root);
}

/// rev-claude-57 seq-shot finding 2 (ADR-7495): the editor-close prompt lists a dirty shot list
/// and Save and exit writes it, like the other asset editors' documents.
#[test]
fn the_close_prompt_lists_a_dirty_shot_list_and_save_and_exit_writes_it() {
    let root = project("close");
    let mut world = world_in(&root);
    ok(&mut world, "editor.shot_panel_open", json!({ "path": REL }));
    let shell = crate::shell::EditorShell::new(None, Some(root.clone()));
    assert!(shell.unsaved_documents(&world).is_empty(), "clean after the open");
    let mut edited = world
        .resource::<ShotPlayer>()
        .sequence()
        .cloned()
        .expect("loaded");
    let first = edited.shots[0].clone();
    edited.shots.push(regia_framework::shots::Shot {
        name: format!("{}_copy", first.name),
        ..first
    });
    world
        .resource_mut::<ShotPlayer>()
        .replace_sequence(edited.clone());
    let listed = shell.unsaved_documents(&world);
    assert_eq!(listed, vec![format!("shot list {REL}")], "{listed:?}");
    let mut shell = shell;
    shell.save_other_documents(&mut world).expect("save and exit");
    assert!(shell.unsaved_documents(&world).is_empty());
    let on_disk = std::fs::read_to_string(root.join(REL)).expect("saved");
    assert_eq!(
        regia_framework::shots::ShotSequence::from_ron(&on_disk).expect("parses"),
        edited,
        "the edit reached the file"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Ctrl+S (Cmd+S) saves the document while the pointer is over the Shots panel, and only then.
#[test]
fn ctrl_s_over_the_panel_saves_the_document() {
    let root = project("ctrl-s");
    let mut world = world_in(&root);
    ok(&mut world, "editor.shot_panel_open", json!({ "path": REL }));
    ok(
        &mut world,
        "editor.shot_panel_edit",
        json!({ "op": "add", "time": 1.0, "value": 50.0 }),
    );
    let ctx = egui::Context::default();
    let frame = |world: &mut World, events: Vec<egui::Event>| {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| crate::shot_panel::shot_panel(ui, world));
        });
        output.textures_delta.clear();
    };
    let ctrl_s = || egui::Event::Key {
        key: egui::Key::S,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND,
    };
    // Pointer outside the window: not the panel's key.
    frame(
        &mut world,
        vec![
            egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
            egui::Event::PointerGone,
            ctrl_s(),
        ],
    );
    assert!(crate::shot_document::is_dirty(&world), "not over the panel");
    frame(
        &mut world,
        vec![egui::Event::PointerMoved(egui::pos2(300.0, 300.0))],
    );
    frame(
        &mut world,
        vec![egui::Event::ModifiersChanged(egui::Modifiers::COMMAND), ctrl_s()],
    );
    assert!(!crate::shot_document::is_dirty(&world), "Ctrl+S saved");
    let saved = std::fs::read_to_string(root.join(REL)).expect("file");
    let list = regia_framework::shots::ShotSequence::from_ron(&saved).expect("list");
    assert_eq!(list.shots[0].camera.fov.0.len(), 2, "the key is in the file");
    let _ = std::fs::remove_dir_all(&root);
}

/// Acceptance (1) through the dispatcher: `editor.create_asset { kind: "sequence" }` and
/// `{ kind: "shot_list" }` write files that open in their editors (the Sequence Curves panel and
/// the Shots panel), clean.
#[test]
fn create_asset_sequence_and_shot_list_open_in_their_panels() {
    let root = regia_test_utils::unique_temp_dir("shot-document-create");
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    let mut world = world_in(&root);
    let made = ok(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "shot_list", "path": "assets/cine/new.shots.ron" }),
    );
    assert_eq!(made["path"], "assets/cine/new.shots.ron", "{made}");
    let opened = ok(
        &mut world,
        "editor.shot_panel_open",
        json!({ "path": "assets/cine/new.shots.ron" }),
    );
    assert_eq!(opened["shots"], 1, "{opened}");
    assert_eq!(opened["dirty"], false, "{opened}");

    let made = ok(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "sequence", "path": "assets/cine/new.seq.ron" }),
    );
    assert_eq!(made["path"], "assets/cine/new.seq.ron", "{made}");
    ok(
        &mut world,
        "editor.seq_curve_open",
        json!({ "path": "assets/cine/new.seq.ron" }),
    );
    let state = ok(&mut world, "editor.seq_curve_state", json!({}));
    assert_eq!(state["path"], "assets/cine/new.seq.ron", "{state}");
    assert_eq!(state["dirty"], false, "{state}");

    let err = call(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "shot_list", "path": "assets/cine/new.shots.ron" }),
    )
    .expect_err("exists");
    assert!(err.message.contains("already exists"), "{}", err.message);
    let _ = std::fs::remove_dir_all(&root);
}

/// One frame of the Shots panel with `events` (the pointer over it).
fn panel_frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        )),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| crate::shot_panel::shot_panel(ui, world));
    });
    output.textures_delta.clear();
}

/// w8 item 1: a list loaded into the `ShotPlayer` by anything but the document (here what
/// `render.sequence { action: "load" }` does) detaches the document: its path is cleared, Save
/// with no path answers `shot_panel_detached`, and neither the agent's Save nor the panel's Save
/// (Ctrl+S) writes the opened file.
#[test]
fn a_list_loaded_after_an_open_detaches_the_document() {
    let root = project("detach");
    let mut world = world_in(&root);
    ok(&mut world, "editor.shot_panel_open", json!({ "path": REL }));
    let a_bytes = std::fs::read(root.join(REL)).expect("file");
    let mut b =
        regia_framework::shots::ShotSequence::from_ron(&String::from_utf8_lossy(&a_bytes)).expect("list");
    b.shots[0].name = "b".into();
    b.shots[0].duration = 7.0;
    let generation = crate::shot_document::generation(&world);
    world.resource_mut::<ShotPlayer>().load(b);

    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["path"], Value::Null, "detached: {state}");
    assert_eq!(state["shots"], json!(["b"]), "{state}");
    assert_eq!(state["dirty"], true, "a list with no file is unsaved: {state}");
    assert_ne!(
        crate::shot_document::generation(&world),
        generation,
        "a new list is a new generation (old cut undo entries do nothing)"
    );
    let err = call(&mut world, "editor.shot_panel_save", json!({})).expect_err("no path");
    assert_eq!(code(err), "shot_panel_detached");
    assert_eq!(
        std::fs::read(root.join(REL)).expect("file"),
        a_bytes,
        "A untouched"
    );

    // The panel's Save (Ctrl+S over the panel) does not write A either.
    let ctx = egui::Context::default();
    panel_frame(
        &ctx,
        &mut world,
        vec![egui::Event::PointerMoved(egui::pos2(300.0, 300.0))],
    );
    panel_frame(
        &ctx,
        &mut world,
        vec![
            egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
            egui::Event::Key {
                key: egui::Key::S,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            },
        ],
    );
    assert_eq!(
        std::fs::read(root.join(REL)).expect("file"),
        a_bytes,
        "A untouched by the panel"
    );
    assert_eq!(
        world.resource::<crate::shot_panel::ShotPanel>().path_text,
        "",
        "the path row no longer names A"
    );

    // Save As gives the list a file of its own.
    let other = "assets/cine/b.shots.ron";
    ok(&mut world, "editor.shot_panel_save", json!({ "path": other }));
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["path"], other, "{state}");
    assert_eq!(state["dirty"], false, "{state}");
    assert_eq!(
        std::fs::read(root.join(REL)).expect("file"),
        a_bytes,
        "A untouched"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// w8 item 2: a sequence player's own shot list (`SequencePlayer::set_shots`, set by the host) is
/// adopted as an unsaved document; a cut edited in the timeline lands in it; Save As writes a
/// `.shots.ron` at the current version; after that it is a document like an opened one (a second
/// save is byte-identical, an edit makes it dirty again); reopened, the cut is there.
#[test]
fn a_host_set_shot_list_is_adopted_saved_as_and_reopened() {
    use regia_framework::sequencer::SequencePlayer;
    use regia_framework::shots::{SHOT_SEQUENCE_FORMAT_VERSION, ShotSequence};

    let root = project("adopt-host");
    let mut world = world_in(&root);
    let text = std::fs::read_to_string(root.join(REL)).expect("file");
    let mut list = ShotSequence::from_ron(&text).expect("list");
    list.shots[0].duration = 5.0;
    // A host may build its list in code with an older version number: Save As writes the current one.
    list.version = 1;
    let sequence = regia_framework::sequencer::edit::tracks::new_sequence("intro", 5.0, 30.0).expect("new");
    let mut owner = SequencePlayer::new(sequence);
    owner.set_shots(Some(list));
    let player = world.spawn(owner).id();
    let camera = world
        .spawn((
            regia_render::Camera::default(),
            regia_scene::Transform {
                translation: regia_core::math::Vec3::new(0.0, 2.0, 6.0),
                ..Default::default()
            },
        ))
        .id();

    let adopted = ok(
        &mut world,
        "editor.shot_panel_adopt",
        json!({ "player": player.to_bits() }),
    );
    assert_eq!(adopted["path"], Value::Null, "{adopted}");
    assert_eq!(adopted["dirty"], true, "{adopted}");
    assert_eq!(adopted["shots"], 1, "{adopted}");
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["host"], json!({ "player": player.to_bits() }), "{state}");

    // The timeline's Add cut edits the player's own list: the document.
    let cut = ok(
        &mut world,
        "editor.seq_cut_edit",
        json!({ "op": "add", "time": 2.0, "camera": camera.to_bits() }),
    );
    assert_eq!(cut["cuts"]["host"], "player", "{cut}");
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["shots"].as_array().map(Vec::len), Some(2), "{state}");
    assert_eq!(state["dirty"], true, "{state}");

    // Save needs a path the first time; Save As writes it.
    let err = call(&mut world, "editor.shot_panel_save", json!({})).expect_err("no path yet");
    assert_eq!(code(err), "shot_panel_bad_path");
    let path = "assets/cine/host.shots.ron";
    ok(&mut world, "editor.shot_panel_save", json!({ "path": path }));
    let state = ok(&mut world, "editor.shot_panel_state", json!({}));
    assert_eq!(state["path"], path, "{state}");
    assert_eq!(state["dirty"], false, "{state}");
    let first = std::fs::read_to_string(root.join(path)).expect("saved");
    assert!(
        first.contains(&format!("version: {SHOT_SEQUENCE_FORMAT_VERSION}")),
        "the current version: {first}"
    );
    ok(&mut world, "editor.shot_panel_save", json!({}));
    assert_eq!(
        std::fs::read_to_string(root.join(path)).expect("saved"),
        first,
        "a second save is byte-identical"
    );
    // Now an ordinary document: an edit through the Shots panel makes it dirty again.
    ok(
        &mut world,
        "editor.shot_panel_edit",
        json!({ "op": "add", "time": 1.0, "value": 50.0 }),
    );
    assert_eq!(
        ok(&mut world, "editor.shot_panel_state", json!({}))["dirty"],
        true
    );
    let in_player = world
        .get::<SequencePlayer>(player)
        .and_then(|p| p.shots().map(|s| s.shots[0].camera.fov.0.len()));
    assert!(
        in_player.is_some_and(|n| n >= 1),
        "the panel edited the player's list"
    );

    // Reopened in a fresh session: the cut is there.
    let mut again = world_in(&root);
    ok(&mut again, "editor.shot_panel_open", json!({ "path": path }));
    let reopened = again
        .resource::<ShotPlayer>()
        .sequence()
        .cloned()
        .expect("loaded");
    assert_eq!(reopened.shots.len(), 2, "the cut survived Save As");
    assert!((reopened.shot_start(1) - 2.0).abs() < 1e-9, "at 2 s");
    assert_eq!(reopened.version, SHOT_SEQUENCE_FORMAT_VERSION);

    // Refusals: no such player, a player with no shot list.
    let err = call(
        &mut world,
        "editor.shot_panel_adopt",
        json!({ "player": camera.to_bits() }),
    )
    .expect_err("not a player");
    assert_eq!(code(err), "shot_panel_no_player");
    let _ = std::fs::remove_dir_all(&root);
}

/// w10 (sprint-w9-rev-v06-lanes § 1 finding 7): the autosave snapshot of a host-built list
/// carries the current format version, as Save writes it, whatever version number the list in
/// memory has; Restore of that snapshot puts the list into the `ShotPlayer` at its file's path
/// (the host player's own list is left as it was).
#[test]
fn the_autosave_of_an_adopted_list_writes_the_current_version_and_restores_into_the_shot_player() {
    use regia_framework::sequencer::SequencePlayer;
    use regia_framework::shots::{SHOT_SEQUENCE_FORMAT_VERSION, ShotSequence};

    let root = project("autosave-adopted");
    let mut world = world_in(&root);
    let text = std::fs::read_to_string(root.join(REL)).expect("file");
    let mut list = ShotSequence::from_ron(&text).expect("list");
    // A host built in code with an older version number.
    list.version = 1;
    let sequence = regia_framework::sequencer::edit::tracks::new_sequence("intro", 5.0, 30.0).expect("new");
    let mut owner = SequencePlayer::new(sequence);
    owner.set_shots(Some(list));
    let player = world.spawn(owner).id();
    ok(
        &mut world,
        "editor.shot_panel_adopt",
        json!({ "player": player.to_bits() }),
    );
    let path = "assets/cine/host.shots.ron";
    ok(&mut world, "editor.shot_panel_save", json!({ "path": path }));
    ok(
        &mut world,
        "editor.shot_panel_edit",
        json!({ "op": "add", "time": 1.0, "value": 50.0 }),
    );
    assert!(crate::shot_document::is_dirty(&world), "edited after the save");

    let snapshots = super::snapshot(&world);
    assert_eq!(snapshots.len(), 1, "a dirty list with a file autosaves");
    let saved_text = String::from_utf8(snapshots[0].bytes.clone()).expect("text");
    assert!(
        saved_text.contains(&format!("version: {SHOT_SEQUENCE_FORMAT_VERSION}")),
        "the autosave carries the current version, as Save does: {saved_text}"
    );
    let recovered = ShotSequence::from_ron(&saved_text).expect("the autosave reads back");
    assert_eq!(recovered.version, SHOT_SEQUENCE_FORMAT_VERSION);
    let in_host = world
        .get::<SequencePlayer>(player)
        .and_then(SequencePlayer::shots)
        .cloned()
        .expect("host list");
    assert_eq!(
        recovered.shots, in_host.shots,
        "the autosave holds the edited list"
    );

    // Restore lands in the ShotPlayer, at the file's path; the host's list is not touched.
    let mut fresh = world_in(&root);
    super::restore(&mut fresh, &snapshots[0].key, &snapshots[0].bytes).expect("restore");
    assert_eq!(crate::shot_document::path(&fresh).as_deref(), Some(path));
    assert_eq!(crate::shot_document::host(&fresh), super::ShotHost::ShotPlayer);
    assert_eq!(
        fresh.resource::<ShotPlayer>().sequence().map(|s| s.shots.clone()),
        Some(in_host.shots),
        "the recovered list is the ShotPlayer's"
    );
    assert!(
        crate::shot_document::is_dirty(&fresh),
        "dirty against the file on disk"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// w10 (sprint-w9-rev-v06-lanes § 1 finding 8): `editor.shot_panel_state` is a read
/// (`mutating: false`, `Permission::Read`): with a detach pending (another list loaded into the
/// player) it reports the detached state but does not commit it; the document's generation,
/// fields and the panel's path row are as they were, and a later mutating method commits it.
#[test]
fn shot_panel_state_reports_a_pending_detach_without_committing_it() {
    let root = project("state-read-only");
    let mut world = world_in(&root);
    world.init_resource::<crate::shot_panel::ShotPanel>();
    ok(&mut world, "editor.shot_panel_open", json!({ "path": REL }));
    // The panel's path row names the file (the draw fills it in).
    world.resource_mut::<crate::shot_panel::ShotPanel>().path_text = REL.to_string();
    let generation = world.resource::<super::ShotDocument>().generation();
    let mut b = world
        .resource::<ShotPlayer>()
        .sequence()
        .cloned()
        .expect("A is loaded");
    b.shots[0].duration += 1.0;
    world.resource_mut::<ShotPlayer>().load(b);
    let panel_path = world
        .get_resource::<crate::shot_panel::ShotPanel>()
        .map(|p| p.path_text.clone());

    for _ in 0..2 {
        let state = ok(&mut world, "editor.shot_panel_state", json!({}));
        assert_eq!(state["path"], Value::Null, "the read sees the detach: {state}");
        assert_eq!(state["dirty"], true, "{state}");
        let doc = world.resource::<super::ShotDocument>();
        assert_eq!(doc.generation(), generation, "the read commits nothing");
        assert_eq!(doc.path(), Some(REL), "the read commits nothing");
        assert_eq!(
            world
                .get_resource::<crate::shot_panel::ShotPanel>()
                .map(|p| p.path_text.clone()),
            panel_path,
            "the panel's path row is untouched by a read"
        );
        // Every other read agrees with the state method.
        assert_eq!(crate::shot_document::path(&world), None);
        assert_eq!(crate::shot_document::generation(&world), generation + 1);
    }
    // A mutating call commits it.
    let err = call(&mut world, "editor.shot_panel_save", json!({})).expect_err("detached");
    assert_eq!(code(err), "shot_panel_detached");
    assert_eq!(world.resource::<super::ShotDocument>().path(), None);
    assert_eq!(
        world.resource::<super::ShotDocument>().generation(),
        generation + 1
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// w10 (sprint-w9-rev-v06-lanes § 1 finding 9): a dirty shot list with no file (detached by
/// another list loaded into the player, or a host list adopted and never saved) is listed by the
/// close prompt as unsaved, says Save and exit cannot save it, and Save and exit goes ahead
/// without it (it used to fail with `shot_panel_detached` / `shot_panel_bad_path`, leaving only
/// Discard). Nothing is written: the old file stays as it was.
#[test]
fn save_and_exit_goes_ahead_past_a_shot_list_with_no_file() {
    use regia_framework::sequencer::SequencePlayer;

    let root = project("close-pathless");
    let mut world = world_in(&root);
    ok(&mut world, "editor.shot_panel_open", json!({ "path": REL }));
    let a_before = std::fs::read(root.join(REL)).expect("A");
    let mut b = world
        .resource::<ShotPlayer>()
        .sequence()
        .cloned()
        .expect("A is loaded");
    b.shots[0].duration += 1.0;
    let mut shell = crate::shell::EditorShell::new(None, Some(root.clone()));

    // Detached: another list loaded into the player.
    world.resource_mut::<ShotPlayer>().load(b.clone());
    let listed = shell.unsaved_documents(&world);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(
        listed[0].contains("untitled shot list") && listed[0].contains("not saved by Save and exit"),
        "listed as unsaved, with no Save offered: {listed:?}"
    );
    shell
        .save_other_documents(&mut world)
        .expect("Save and exit goes ahead");
    assert_eq!(std::fs::read(root.join(REL)).expect("A"), a_before, "A untouched");
    assert_eq!(
        world.resource::<ShotPlayer>().sequence(),
        Some(&b),
        "the list in the player is left as it is"
    );

    // Adopted: a host's own list, never saved.
    let sequence = regia_framework::sequencer::edit::tracks::new_sequence("intro", 5.0, 30.0).expect("new");
    let mut owner = SequencePlayer::new(sequence);
    owner.set_shots(Some(b.clone()));
    let player = world.spawn(owner).id();
    ok(
        &mut world,
        "editor.shot_panel_adopt",
        json!({ "player": player.to_bits(), "discard": true }),
    );
    let listed = shell.unsaved_documents(&world);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(listed[0].contains("not saved by Save and exit"), "{listed:?}");
    shell
        .save_other_documents(&mut world)
        .expect("Save and exit goes ahead");
    assert_eq!(std::fs::read(root.join(REL)).expect("A"), a_before, "A untouched");
    let _ = std::fs::remove_dir_all(&root);
}
