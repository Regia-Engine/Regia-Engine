// SPDX-License-Identifier: MPL-2.0
//! Match-mode authoring in the editor (ADR-8738, `book/agents/editor-match-rules.md`): the Game
//! Rules picker of World Settings, the Match Rules panel's rule-set form and playlist editor, the
//! match-rules overlays and the play-in-editor Match strip. Every test drives the `editor.*`
//! methods by name (the same handlers the panel's controls call) and the editor loop's
//! `apply_request` / `pump` path; nothing waits on a clock.

use std::panic::{AssertUnwindSafe, catch_unwind};

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_editor::command::{UndoStack, redo_world, undo_world};
use regia_editor::level_io;
use regia_editor::match_rules_play::{MatchLive, MatchPlayState, SpawnHeat, overlays};
use regia_editor::overlays::{ICON_PLAYER_START, OverlayShape, build_overlays};
use regia_editor::pie::{PieRequest, PieSession, PieStatus, apply_request, sync_status};
use regia_editor::selection::Selection;
use regia_editor::viewport::EditorCamera;
use regia_editor::views::{ShowFlags, ViewCamera, ViewKind};
use regia_framework::player::{PlayerStart, SpawnPoint};
use regia_framework::rules::MatchRules;
use regia_framework::rules::objective::{Objective, ObjectiveKind};
use regia_framework::rules::teams::default_team_defs;
use regia_net::formats::PlaylistFile;
use regia_scene::GlobalTransform;
use regia_scene::level::current_settings;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

fn edit_app(root: Option<&std::path::Path>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<PieStatus>();
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: root.map(std::path::Path::to_path_buf),
        });
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

/// Calls an editor agent method by name; an error answers its `data.reason` (else `data.code`).
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, (String, Value)> {
    if world.get_resource::<AgentExtension>().is_none() {
        regia_editor::agent_ext::install(world);
    }
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).map_err(|e| {
        let data = e.data.clone().unwrap_or(Value::Null);
        let code = data
            .get("reason")
            .or_else(|| data.get("code"))
            .and_then(Value::as_str)
            .map_or_else(|| e.message.clone(), str::to_owned);
        (code, data)
    })
}

fn ok(world: &mut World, method: &str, params: Value) -> Value {
    call(world, method, params.clone())
        .unwrap_or_else(|(code, data)| panic!("{method} {params}: {code} {data}"))
}

fn code(world: &mut World, method: &str, params: Value) -> (String, Value) {
    match call(world, method, params.clone()) {
        Ok(out) => panic!("{method} {params} should be refused: {out}"),
        Err(refusal) => refusal,
    }
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Every text egui painted in `shapes`.
fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
    fn walk(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => out.push(text.galley.text().to_owned()),
            egui::Shape::Vec(list) => list.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for clipped in shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

/// Draws `panel` for `frames` frames on a headless egui context and answers the last frame's texts.
fn draw(world: &mut World, frames: usize, panel: fn(&mut egui::Ui, &mut World)) -> Vec<String> {
    let ctx = egui::Context::default();
    let mut shown = Vec::new();
    for _ in 0..frames {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 3000.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| panel(ui, world));
        output.textures_delta.clear();
        shown = texts(&output.shapes);
    }
    shown
}

fn write(root: &std::path::Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, text).expect("write");
}

const CTF_LONG: &str = r#"(
    format_version: 1,
    id: "ctf_long",
    extends: "ctf",
    session: ( objectives: ( capture_limit: 5 ) ),
    options: [ (key: "capture_limit", kind: int, default: "5", min: "1", max: "10") ],
)"#;

/// GR-7.1: Game Rules is a picker fed by `GameRulesRegistry::registered_names` and the project's
/// rule sets; a name in neither is kept as custom text with the `game_rules_unregistered` chip.
#[test]
fn level_settings_game_rules_offers_registered_names() {
    let root = regia_test_utils::unique_temp_dir("match-rules-picker");
    write(&root, "assets/rules/ctf_long.rules.ron", CTF_LONG);
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let mut registry = regia_framework::rules::GameRulesRegistry::default();
    registry.register::<regia_framework::rules::DefaultRules>("my_game::ArenaRules");
    world.insert_resource(registry);

    let got = ok(world, "editor.world_settings_get", json!({}));
    let picker = &got["game_rules"];
    let registered = names(&picker["registered"]);
    for name in ["my_game::ArenaRules", "DefaultRules", "MatchSessionRules"] {
        assert!(registered.iter().any(|n| n == name), "{name} in {registered:?}");
    }
    let rule_sets = names(&picker["rule_sets"]);
    for id in ["ctf", "ctf_long", "koth", "tdm"] {
        assert!(rule_sets.iter().any(|n| n == id), "{id} in {rule_sets:?}");
    }
    assert_eq!(picker["status"], "none");
    assert_eq!(picker["warning"], Value::Null);

    // The combo's pick is one undo step; undo restores the level's own rules.
    assert!(regia_editor::match_rules_panel::pick_game_rules(
        world,
        Some("my_game::ArenaRules".into())
    ));
    let got = ok(world, "editor.world_settings_get", json!({}));
    assert_eq!(got["game_rules"]["current"], "my_game::ArenaRules");
    assert_eq!(got["game_rules"]["status"], "registered");
    assert!(regia_editor::match_rules_panel::pick_game_rules(
        world,
        Some("ctf_long".into())
    ));
    assert_eq!(
        ok(world, "editor.world_settings_get", json!({}))["game_rules"]["status"],
        "rule_set"
    );
    assert!(undo_world(world));
    assert_eq!(
        current_settings(world).game_rules.as_deref(),
        Some("my_game::ArenaRules")
    );
    assert!(undo_world(world));
    assert_eq!(current_settings(world).game_rules, None);

    // A typed name nobody registered: kept, marked custom, with the warning code.
    ok(
        world,
        "editor.world_settings_set",
        json!({ "settings": { "game_rules": "typo_rules" } }),
    );
    let got = ok(world, "editor.world_settings_get", json!({}));
    assert_eq!(got["game_rules"]["current"], "typo_rules");
    assert_eq!(got["game_rules"]["status"], "custom");
    assert_eq!(got["game_rules"]["warning"], "game_rules_unregistered");

    // The panel draws the picker and the chip.
    let shown = draw(world, 2, regia_editor::world_settings::world_settings_panel);
    assert!(shown.iter().any(|t| t == "Game Rules"), "{shown:?}");
    assert!(shown.iter().any(|t| t.contains("custom")), "{shown:?}");
    assert!(shown.iter().any(|t| t.contains("typo_rules")), "{shown:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// GR-7.2: the rule-set form edits fields through `editor.rules_set_field`; each edit is one undo
/// step, undo and redo restore the values, and save writes a file the loader reads back.
#[test]
fn match_rules_panel_edits_a_ruleset_and_undo_restores_it() {
    let root = regia_test_utils::unique_temp_dir("match-rules-edit");
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let dirty_before = world.resource::<UndoStack>().is_dirty();
    let path = "assets/rules/ctf_long.rules.ron";
    let opened = ok(
        world,
        "editor.rules_open",
        json!({ "path": path, "create": true, "extends": "ctf" }),
    );
    assert_eq!(opened["rules"]["id"], "ctf_long");
    assert_eq!(opened["rules"]["new"], true);
    assert_eq!(opened["rules"]["errors"], json!([]));
    let field = |state: &Value, path: &str| -> Value {
        state["rules"]["fields"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["path"] == path))
            .map(|r| r["value"].clone())
            .unwrap_or_else(|| panic!("no form row {path}: {state}"))
    };
    assert_eq!(field(&opened, "session.objectives.capture_limit"), Value::Null);
    assert_eq!(field(&opened, "extends"), "ctf");

    let set = ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "session.objectives.capture_limit", "value": 7 }),
    );
    assert_eq!(set["changed"], true);
    assert_eq!(field(&set, "session.objectives.capture_limit"), 7);
    let set = ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "name_key", "value": "mode.ctf_long" }),
    );
    assert_eq!(field(&set, "name_key"), "mode.ctf_long");
    let (refused, _) = code(
        world,
        "editor.rules_set_field",
        json!({ "field": "session.objectives.capture_limt", "value": 7 }),
    );
    assert_eq!(refused, "match_rules_bad_field");

    // Undo walks back one field at a time; redo puts it back.
    assert!(undo_world(world));
    let state = ok(world, "editor.match_rules_state", json!({}));
    assert_eq!(field(&state, "name_key"), "");
    assert_eq!(field(&state, "session.objectives.capture_limit"), 7);
    assert!(undo_world(world));
    let state = ok(world, "editor.match_rules_state", json!({}));
    assert_eq!(field(&state, "session.objectives.capture_limit"), Value::Null);
    assert!(redo_world(world));
    let state = ok(world, "editor.match_rules_state", json!({}));
    assert_eq!(field(&state, "session.objectives.capture_limit"), 7);
    assert_eq!(
        world.resource::<UndoStack>().is_dirty(),
        dirty_before,
        "a rule-set edit is a file edit, not a level edit"
    );

    let saved = ok(world, "editor.rules_save", json!({}));
    assert_eq!(saved["written"], path);
    assert_eq!(saved["rules"]["dirty"], false);
    let text = std::fs::read_to_string(root.join(path)).expect("saved");
    let back = regia_framework::ruleset_file::parse_rules_ron(&text).expect("the loader reads it");
    assert_eq!(back.session.objectives.capture_limit, Some(7));
    assert_eq!(back.extends.as_deref(), Some("ctf"));
    // The library reloaded: the picker lists the new id.
    let picker = ok(world, "editor.world_settings_get", json!({}));
    assert!(names(&picker["game_rules"]["rule_sets"]).contains(&"ctf_long".to_owned()));

    // Reopen from disk: the saved value.
    let reopened = ok(world, "editor.rules_open", json!({ "path": path }));
    assert_eq!(field(&reopened, "session.objectives.capture_limit"), 7);
    assert_eq!(reopened["rules"]["new"], false);
    let shown = draw(world, 2, regia_editor::match_rules_panel::match_rules_panel);
    assert!(
        shown.iter().any(|t| t == "session.objectives.capture_limit"),
        "the form lists the format's fields: {shown:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// GR-7.2: an option whose default is outside its range shows `ruleset_option_range` inline and
/// in the panel, and save is refused (`match_rules_save_blocked`) until it is fixed.
#[test]
fn an_invalid_option_shows_its_code_and_blocks_save() {
    let root = regia_test_utils::unique_temp_dir("match-rules-invalid");
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let path = "assets/rules/laps.rules.ron";
    ok(
        world,
        "editor.rules_open",
        json!({ "path": path, "create": true, "skeleton": "custom" }),
    );
    let state = ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "options[0]", "value": {
            "key": "laps", "kind": "int", "default": "0", "min": "1", "max": "9", "choices": []
        } }),
    );
    let errors = state["rules"]["errors"].as_array().cloned().unwrap_or_default();
    assert_eq!(errors.len(), 1, "{state}");
    assert_eq!(errors[0]["code"], "ruleset_option_range");
    assert!(
        errors[0]["path"]
            .as_str()
            .unwrap_or("")
            .contains("options[0].default"),
        "{errors:?}"
    );
    assert_eq!(state["rules"]["can_save"], false);

    let (refused, data) = code(world, "editor.rules_save", json!({}));
    assert_eq!(refused, "match_rules_save_blocked");
    assert_eq!(data["errors"][0]["code"], "ruleset_option_range");
    assert!(!root.join(path).exists(), "a refused save writes nothing");

    let shown = draw(world, 2, regia_editor::match_rules_panel::match_rules_panel);
    assert!(
        shown.iter().any(|t| t.contains("ruleset_option_range")),
        "the panel shows the code: {shown:?}"
    );

    // A kind error has its own code.
    let state = ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "options[0].default", "value": "three" }),
    );
    assert_eq!(state["rules"]["errors"][0]["code"], "ruleset_option_kind");
    // Fixed: no problem, and the save goes through.
    let state = ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "options[0].default", "value": "3" }),
    );
    assert_eq!(state["rules"]["errors"], json!([]));
    ok(world, "editor.rules_save", json!({}));
    assert!(root.join(path).is_file());
    let _ = std::fs::remove_dir_all(&root);
}

/// integ-rules-v07b: the editor saves only playlists its own reader reads. Playlist rotation
/// (ADR-8897) made `PlaylistFile::from_ron` refuse a row whose `min_players` bound is above its
/// `max_players` bound, read from the field or from the row option of that name. A row on a game's
/// own mode skips the rule-set option checks, so before this fix the editor wrote such a row and
/// could not open the file again.
#[test]
fn playlist_editor_blocks_a_row_its_reader_refuses() {
    let root = regia_test_utils::unique_temp_dir("match-rules-playlist-gates");
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let path = "assets/gates.playlist.ron";
    ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "open", "path": path, "create": true }),
    );
    let state = ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "add", "row": {
            "map_id": "levels/a.level.ron",
            "mode": "my_mode",
            "options": { "min_players": "8", "max_players": "4" },
        } }),
    );
    assert_eq!(state["playlist"]["can_save"], false, "{state}");
    let (refused, data) = code(world, "editor.playlist_edit", json!({ "op": "save" }));
    assert_eq!(refused, "playlist_save_blocked", "{data}");
    assert!(
        data["problems"].as_array().is_some_and(|p| p
            .iter()
            .any(|p| p["code"] == "playlist_bad_row" && p["path"] == "max_players")),
        "{data}"
    );
    assert!(!root.join(path).exists(), "nothing written");
    // The fixed bounds save, and the reader reads the file back.
    ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "set_row", "index": 0, "row": { "options": { "min_players": "4", "max_players": "8" } } }),
    );
    let saved = ok(world, "editor.playlist_edit", json!({ "op": "save" }));
    assert_eq!(saved["written"], path);
    let text = std::fs::read_to_string(root.join(path)).expect("saved");
    let file = PlaylistFile::from_ron(&text).expect("the reader reads what the editor saved");
    assert_eq!(file.entries[0].max_players_bound(), Some(8));
    let _ = std::fs::remove_dir_all(&root);
}

/// GR-7.2: the playlist editor adds, reorders and edits rows as undo steps, validates each row's
/// options against its rule set, refuses a save with a blocking problem and writes atomically.
#[test]
fn playlist_editor_reorders_rows_and_saves_atomically() {
    let root = regia_test_utils::unique_temp_dir("match-rules-playlist");
    write(&root, "assets/rules/ctf_long.rules.ron", CTF_LONG);
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let path = "assets/rotation.playlist.ron";
    ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "open", "path": path, "create": true }),
    );
    for (map, mode) in [
        ("levels/a.level.ron", "tdm"),
        ("levels/b.level.ron", "ctf_long"),
        ("levels/c.level.ron", "my_mode"),
    ] {
        ok(
            world,
            "editor.playlist_edit",
            json!({ "op": "add", "row": { "map_id": map, "mode": mode } }),
        );
    }
    let state = ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "move", "from": 2, "to": 0 }),
    );
    let order = |state: &Value| -> Vec<String> {
        state["playlist"]["rows"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r["map_id"].as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        order(&state),
        ["levels/c.level.ron", "levels/a.level.ron", "levels/b.level.ron"]
    );
    // A game's own mode is a warning, never a blocker.
    let problems = state["playlist"]["problems"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0]["code"], "playlist_mode_unknown");
    assert_eq!(problems[0]["blocking"], false);
    assert_eq!(state["playlist"]["can_save"], true);

    // A row option outside the rule set's range blocks the save.
    let state = ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "set_row", "index": 2, "row": { "options": { "capture_limit": "99" } } }),
    );
    assert!(
        state["playlist"]["problems"].as_array().is_some_and(|p| p
            .iter()
            .any(|p| p["code"] == "ruleset_option_range" && p["blocking"] == true)),
        "{state}"
    );
    let (refused, data) = code(world, "editor.playlist_edit", json!({ "op": "save" }));
    assert_eq!(refused, "playlist_save_blocked");
    assert_eq!(data["problems"][0]["code"], "ruleset_option_range");
    assert!(!root.join(path).exists());
    let state = ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "set_row", "index": 2, "row": { "options": { "speed": "2" } } }),
    );
    assert!(
        state["playlist"]["problems"]
            .as_array()
            .is_some_and(|p| p.iter().any(|p| p["code"] == "ruleset_option_unknown")),
        "{state}"
    );
    ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "set_row", "index": 2, "row": { "options": { "capture_limit": "8" } } }),
    );
    let saved = ok(world, "editor.playlist_edit", json!({ "op": "save" }));
    assert_eq!(saved["written"], path);

    // On disk: the reordered rows, in the format the server reads, and no temp file left behind.
    let file =
        PlaylistFile::from_ron(&std::fs::read_to_string(root.join(path)).expect("saved")).expect("reads");
    let maps: Vec<&str> = file.entries.iter().map(|e| e.map_id.as_str()).collect();
    assert_eq!(
        maps,
        ["levels/c.level.ron", "levels/a.level.ron", "levels/b.level.ron"]
    );
    assert_eq!(
        file.entries[2].options.get("capture_limit").map(String::as_str),
        Some("8")
    );
    let left: Vec<String> = std::fs::read_dir(root.join("assets"))
        .expect("dir")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert!(
        left.iter().all(|n| n == "rotation.playlist.ron" || n == "rules"),
        "only the playlist (and the rules folder): {left:?}"
    );

    // Undo: the three option edits, then the move; the rows are back in their first order.
    for _ in 0..4 {
        assert!(undo_world(world));
    }
    let state = ok(world, "editor.match_rules_state", json!({}));
    assert_eq!(
        order(&state),
        ["levels/a.level.ron", "levels/b.level.ron", "levels/c.level.ron"]
    );
    assert_eq!(
        state["playlist"]["dirty"], true,
        "the file still holds the saved order"
    );
    let (bad, _) = code(
        world,
        "editor.playlist_edit",
        json!({ "op": "move", "from": 0, "to": 9 }),
    );
    assert_eq!(bad, "playlist_bad_row");
    let (bad, _) = code(world, "editor.playlist_edit", json!({ "op": "shuffle" }));
    assert_eq!(bad, "playlist_bad_op");
    let _ = std::fs::remove_dir_all(&root);
}

fn spawn_start(world: &mut World, at: Vec3, point: Option<SpawnPoint>) -> Entity {
    let transform = Transform::from_translation(at);
    let mut entity = world.spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        PlayerStart,
    ));
    if let Some(point) = point {
        entity.insert(point);
    }
    entity.id()
}

fn line_colors(shapes: &[OverlayShape]) -> Vec<egui::Color32> {
    shapes
        .iter()
        .filter_map(|s| match s {
            OverlayShape::Line { color, .. } => Some(*color),
            _ => None,
        })
        .collect()
}

fn overlay_texts(shapes: &[OverlayShape]) -> Vec<String> {
    shapes
        .iter()
        .filter_map(|s| match s {
            OverlayShape::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn team(id: u8) -> egui::Color32 {
    let def = default_team_defs()
        .into_iter()
        .find(|d| d.id == id)
        .expect("a default team");
    egui::Color32::from_rgba_unmultiplied(def.color[0], def.color[1], def.color[2], 255)
}

/// GR-7.3/7.4: under Show ▸ Icons ▸ Match rules a spawn point draws in its team's color with a team
/// and priority label, a start for every team keeps the player-start color, and a flag base draws
/// its touch radius in its team's color. With the toggle off, nothing is tinted (the control).
#[test]
fn spawn_points_draw_in_their_team_color() {
    let mut world = World::new();
    world.insert_resource(EditorCamera {
        target: Vec3::ZERO,
        distance: 12.0,
        ..EditorCamera::default()
    });
    spawn_start(
        &mut world,
        Vec3::new(1.0, 0.0, 0.0),
        Some(SpawnPoint {
            team: 2,
            priority: 5,
            ..SpawnPoint::default()
        }),
    );
    spawn_start(&mut world, Vec3::new(-1.0, 0.0, 0.0), None);
    let view = ViewCamera::of(ViewKind::Perspective, world.resource::<EditorCamera>());
    let size = Vec2::new(640.0, 480.0);
    let on = ShowFlags::default();
    assert!(
        on.icons && on.icon_filter.match_rules,
        "the toggle is on by default"
    );
    let shapes = build_overlays(&world, &view, ViewKind::Perspective, size, on);
    let colors = line_colors(&shapes);
    assert!(colors.contains(&team(2)), "team 2's start is tinted");
    assert!(
        colors.contains(&ICON_PLAYER_START),
        "the start for every team keeps its color"
    );
    assert!(
        overlay_texts(&shapes).iter().any(|t| t == "team 2 · p5"),
        "{:?}",
        overlay_texts(&shapes)
    );

    let mut off = on;
    off.icon_filter.match_rules = false;
    let shapes = build_overlays(&world, &view, ViewKind::Perspective, size, off);
    assert!(
        !line_colors(&shapes).contains(&team(2)),
        "the control: no tint with the toggle off"
    );
    assert!(overlay_texts(&shapes).iter().all(|t| !t.starts_with("team ")));

    // A flag base of team 1 on the edit world's board.
    let mut rules = MatchRules::default();
    rules.session.board.objectives.push(Objective {
        entity_id: 1,
        kind: ObjectiveKind::Flag,
        team_id: 1,
        carrier_id: None,
        at_base: true,
        base_position: [0.0, 0.0, 2.0],
        position: [0.0, 0.0, 2.0],
        volume: None,
        dropped_tick: None,
        zone: None,
    });
    world.insert_resource(rules);
    let shapes = build_overlays(&world, &view, ViewKind::Perspective, size, on);
    let team1 = line_colors(&shapes).iter().filter(|c| **c == team(1)).count();
    assert!(
        team1 >= 20,
        "the flag's touch circle in team 1's color: {team1} segments"
    );
    assert!(
        overlay_texts(&shapes)
            .iter()
            .any(|t| t == "flag 1 · team 1 · home")
    );
    let shapes = build_overlays(&world, &view, ViewKind::Perspective, size, off);
    assert!(!line_colors(&shapes).contains(&team(1)));
}

fn start_play(app: &mut App) -> Option<PieSession> {
    let mut slot = None;
    let line = apply_request(app.world_mut(), &mut slot, PieRequest::Play, None);
    assert!(slot.is_some(), "play started: {line}");
    sync_status(app.world_mut(), slot.as_ref());
    slot
}

/// One editor frame of the Match strip: the queued controls run on the play world.
fn control(world: &mut World, slot: &mut Option<PieSession>, params: Value) -> Value {
    let queued = ok(world, "editor.match_control", params);
    assert_eq!(queued["queued"], true);
    regia_editor::match_rules_play::pump(world, slot.as_mut());
    let state = ok(world, "editor.match_rules_state", json!({}));
    assert_eq!(state["match"]["queued"], 0);
    assert_eq!(state["match"]["last_control"]["id"], queued["id"]);
    state["match"].clone()
}

/// GR-7.5: the Match strip's controls (`editor.match_control`) start, pause, step N ticks while
/// paused, seat and unseat bots, force the next row and end the match in the play world.
#[test]
fn pie_match_strip_pauses_steps_and_ends_the_match_through_editor_match_control() {
    let mut app = edit_app(None);
    let (refused, _) = code(
        app.world_mut(),
        "editor.match_control",
        json!({ "action": "start" }),
    );
    assert_eq!(refused, "match_control_not_playing");
    let mut slot = start_play(&mut app);
    let world = app.world_mut();
    let (refused, _) = code(world, "editor.match_control", json!({ "action": "dance" }));
    assert_eq!(refused, "match_control_bad_action");
    let (refused, _) = code(
        world,
        "editor.match_control",
        json!({ "action": "step", "steps": 0 }),
    );
    assert_eq!(refused, "match_control_bad_action");

    // Before start the play world has no session.
    let state = control(world, &mut slot, json!({ "action": "pause" }));
    assert_eq!(state["last_control"]["code"], "match_control_no_session");
    let state = control(world, &mut slot, json!({ "action": "start" }));
    assert_eq!(state["last_control"]["ok"], true, "{state}");
    assert_eq!(state["session"], true);
    assert_eq!(state["live"]["mode"], "tdm");
    assert_eq!(state["live"]["paused"], false);
    let tick0 = state["live"]["tick"].as_u64().expect("tick");

    let state = control(world, &mut slot, json!({ "action": "step", "steps": 5 }));
    assert_eq!(state["last_control"]["code"], "match_control_not_paused");
    let state = control(world, &mut slot, json!({ "action": "pause" }));
    assert_eq!(state["live"]["paused"], true);
    let state = control(world, &mut slot, json!({ "action": "step", "steps": 5 }));
    assert_eq!(state["last_control"]["ok"], true, "{state}");
    assert_eq!(state["live"]["tick"].as_u64(), Some(tick0 + 5), "five ticks ran");
    assert_eq!(state["live"]["paused"], true, "still paused after the step");
    let play_tick = slot
        .as_ref()
        .and_then(|s| s.app.world().get_resource::<MatchRules>())
        .map(|r| r.session.flow.match_tick);
    assert_eq!(play_tick, Some(tick0 + 5), "the play world's session, not a copy");

    let state = control(world, &mut slot, json!({ "action": "add_bot", "team": 1 }));
    assert_eq!(state["bots"], json!([{ "team": 1, "slot": 0 }]));
    assert!(
        state["live"]["seated"]
            .as_array()
            .is_some_and(|s| s.contains(&json!({ "team": 1, "slot": 0 })))
    );
    let state = control(world, &mut slot, json!({ "action": "remove_bot", "team": 1 }));
    assert_eq!(state["bots"], json!([]));
    let state = control(world, &mut slot, json!({ "action": "remove_bot", "team": 1 }));
    assert_eq!(state["last_control"]["code"], "match_control_no_bot");
    let state = control(world, &mut slot, json!({ "action": "next_row", "row": 9 }));
    assert_eq!(state["last_control"]["code"], "match_control_bad_row");
    let state = control(world, &mut slot, json!({ "action": "next_row", "row": 0 }));
    assert_eq!(state["last_control"]["ok"], true);

    let state = control(world, &mut slot, json!({ "action": "resume" }));
    assert_eq!(state["live"]["paused"], false);
    let state = control(world, &mut slot, json!({ "action": "end" }));
    assert_eq!(state["last_control"]["ok"], true, "{state}");
    assert_eq!(state["live"]["between"], true, "the match ended");

    // The strip draws while playing (its buttons call the same method).
    let shown = draw(world, 2, regia_editor::match_rules_play::match_strip);
    assert!(
        shown.iter().any(|t| t == "Pause") && shown.iter().any(|t| t == "End match"),
        "{shown:?}"
    );

    // Stop: the controls are refused again and the live view is gone.
    apply_request(world, &mut slot, PieRequest::Stop, None);
    sync_status(world, slot.as_ref());
    regia_editor::match_rules_play::pump(world, slot.as_mut());
    let (refused, _) = code(world, "editor.match_control", json!({ "action": "pause" }));
    assert_eq!(refused, "match_control_not_playing");
    assert_eq!(
        ok(world, "editor.match_rules_state", json!({}))["match"]["session"],
        false
    );
}

/// Rule 8: the panel's controls are all classified in the parity allowlist with a real agent path,
/// so the scan has no gap for them; the new methods are registered, named in `agent.guide` and
/// listed in the editor's method snapshot.
#[test]
fn agent_parity_scan_has_no_gap_for_the_match_rules_panel() {
    let allow = regia_editor::agent_parity::allowlist().expect("allowlist");
    let ours: Vec<_> = allow
        .iter()
        .filter(|e| e.file == "match_rules_panel.rs" || e.file == "match_rules_play.rs")
        .collect();
    assert!(
        ours.len() >= 10,
        "the panel's controls are scanned: {}",
        ours.len()
    );
    for entry in &ours {
        assert_ne!(
            entry.class,
            regia_editor::agent_parity::SiteClass::Gap,
            "{}:{} `{}` has no agent path",
            entry.file,
            entry.line,
            entry.text
        );
    }
    let methods = [
        "editor.match_rules_state",
        "editor.rules_open",
        "editor.rules_set_field",
        "editor.rules_save",
        "editor.playlist_edit",
        "editor.match_control",
    ];
    let mut world = World::new();
    regia_editor::agent_ext::install(&mut world);
    let extension = world.resource::<AgentExtension>().clone();
    for method in methods {
        assert!(extension.method(method).is_some(), "{method} is registered");
        assert!(regia_agent::guide_mentions(method), "agent.guide names {method}");
    }
    for entry in ours
        .iter()
        .filter(|e| e.class == regia_editor::agent_parity::SiteClass::Method)
    {
        assert!(
            methods.contains(&entry.via.as_str()) || entry.via.starts_with("editor."),
            "{}: via {}",
            entry.text,
            entry.via
        );
    }
    let snapshot = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/api-snapshots/editor-agent-methods.txt"),
    )
    .expect("snapshot");
    for method in methods {
        assert!(snapshot.contains(method), "{method} is in the method snapshot");
    }
}

/// The user guide's screenshots, taken through the agent protocol (`editor.capture_panel`): the Match
/// Rules panel with a blocking option problem and after the fix, and World Settings with the Game
/// Rules picker and its custom-name chip. Not a golden: the captures must decode, have the asked
/// size, and differ where the panel's content differs. With `REGIA_MATCH_RULES_SHOTS=<dir>` the PNGs
/// are written there (atomically). Without a Vulkan adapter (`F0011`) the test says so and stops.
#[test]
fn match_rules_panel_screenshots_through_the_agent() {
    let root = regia_test_utils::unique_temp_dir("match-rules-shots");
    write(&root, "assets/rules/ctf_long.rules.ron", CTF_LONG);
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let capture = |world: &mut World, panel: &str| -> Option<Value> {
        match call(
            world,
            "editor.capture_panel",
            json!({ "panel": panel, "width": 1000, "height": 900 }),
        ) {
            Ok(shot) => Some(shot),
            Err((code, data)) if code == "F0011" || code == "editor_headless" => {
                eprintln!("no Vulkan adapter, screenshots not taken: {data}");
                None
            }
            Err((code, data)) => panic!("capture {panel}: {code} {data}"),
        }
    };
    ok(
        world,
        "editor.rules_open",
        json!({ "path": "assets/rules/laps.rules.ron", "create": true, "extends": "ctf_long" }),
    );
    ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "options[0]", "value": {
            "key": "laps", "kind": "int", "default": "0", "min": "1", "max": "9", "choices": []
        } }),
    );
    let Some(blocked) = capture(world, "match_rules") else {
        return;
    };
    ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "options[0].default", "value": "3" }),
    );
    let fixed = capture(world, "match_rules").expect("a second capture");
    ok(
        world,
        "editor.world_settings_set",
        json!({ "settings": { "game_rules": "typo_rules" } }),
    );
    let picker = capture(world, "world_settings").expect("world settings");
    for shot in [&blocked, &fixed, &picker] {
        assert_eq!(shot["mime"], "image/png");
        assert_eq!(shot["width"], 1000);
        assert_eq!(shot["height"], 900);
    }
    assert_ne!(
        blocked["png_base64"], fixed["png_base64"],
        "the problem row is drawn"
    );
    if let Some(dir) = std::env::var_os("REGIA_MATCH_RULES_SHOTS") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).expect("shot dir");
        for (name, shot) in [
            ("match-rules-panel-blocked.png", &blocked),
            ("match-rules-panel-fixed.png", &fixed),
            ("world-settings-game-rules.png", &picker),
        ] {
            let bytes = regia_editor::reference::base64_decode(shot["png_base64"].as_str().expect("png"))
                .expect("base64");
            let pending = dir.join(format!("{name}.tmp"));
            std::fs::write(&pending, bytes).expect("write");
            std::fs::rename(&pending, dir.join(name)).expect("publish");
            eprintln!("MATCH_RULES_SHOT={}", dir.join(name).display());
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

// Review proofs for buildout-rules-editor-authoring d3c422a57. Each test asserts the behavior a
// fix lane should ship, so these fail on that head on purpose.

const PLAIN_RULES: &str = "(\n    format_version: 1,\n    id: \"plain\",\n    skeleton: \"tdm\",\n)\n";

const PLAIN_PLAYLIST: &str =
    "(format_version: 1, entries: [(map_id: \"levels/arena.level.ron\", mode: \"tdm\")])\n";

fn file_bytes(root: &std::path::Path, rel: &str) -> Vec<u8> {
    std::fs::read(root.join(rel)).expect("read back")
}

/// A clean open of a hand-written `.rules.ron` stays byte-identical across Save.
#[test]
fn review_unchanged_rules_save_is_byte_identical() {
    let root = regia_test_utils::unique_temp_dir("review-rules-bytes");
    write(&root, "assets/rules/plain.rules.ron", PLAIN_RULES);
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let state = ok(
        world,
        "editor.rules_open",
        json!({ "path": "assets/rules/plain.rules.ron" }),
    );
    assert_eq!(state["rules"]["dirty"], false, "opening did not edit the set");
    ok(world, "editor.rules_save", json!({}));
    assert_eq!(
        file_bytes(&root, "assets/rules/plain.rules.ron"),
        PLAIN_RULES.as_bytes(),
        "save of an unchanged rule set rewrote the file"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A clean open of a hand-written `.playlist.ron` stays byte-identical across Save.
#[test]
fn review_unchanged_playlist_save_is_byte_identical() {
    let root = regia_test_utils::unique_temp_dir("review-playlist-bytes");
    write(&root, "assets/rotation.playlist.ron", PLAIN_PLAYLIST);
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let state = ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "open", "path": "assets/rotation.playlist.ron" }),
    );
    assert_eq!(
        state["playlist"]["dirty"], false,
        "opening did not edit the playlist"
    );
    ok(world, "editor.playlist_edit", json!({ "op": "save" }));
    assert_eq!(
        file_bytes(&root, "assets/rotation.playlist.ron"),
        PLAIN_PLAYLIST.as_bytes(),
        "save of an unchanged playlist rewrote the file"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A hostile file under `assets/rules/` is kept, with its `ruleset_*` code, on `editor.match_rules_state`.
#[test]
fn review_hostile_rules_file_is_named_in_match_rules_state() {
    let root = regia_test_utils::unique_temp_dir("review-hostile-rules");
    write(&root, "assets/rules/evil.rules.ron", "this is not a rule set\n");
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let state = ok(world, "editor.match_rules_state", json!({}));
    let recorded = world
        .resource::<regia_editor::match_rules_panel::ProjectRules>()
        .registry
        .errors()
        .iter()
        .any(|(_, error)| error.code == "ruleset_parse");
    assert!(
        recorded,
        "the loader recorded ruleset_parse for assets/rules/evil.rules.ron"
    );
    let blob = state.to_string();
    assert!(
        blob.contains("ruleset_parse"),
        "editor.match_rules_state must name the folder load error, got {blob}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn rules_field<'a>(state: &'a Value, path: &str) -> Option<&'a Value> {
    state["rules"]["fields"]
        .as_array()?
        .iter()
        .find(|row| row["path"] == path)
        .map(|row| &row["value"])
}

/// `editor.rules_open` is an edit: it refuses to replace a dirty document, and undo still restores it.
#[test]
fn review_rules_open_refuses_a_dirty_document() {
    let mut gaps = Vec::new();
    let open = regia_editor::match_rules_panel::methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.rules_open")
        .expect("editor.rules_open");
    if !open.0.mutating || open.0.permission.name() != "edit_scene" {
        gaps.push(format!(
            "editor.rules_open is mutating={} permission={} (want mutating, edit_scene)",
            open.0.mutating,
            open.0.permission.name()
        ));
    }

    let root = regia_test_utils::unique_temp_dir("review-dirty-open");
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    ok(
        world,
        "editor.rules_open",
        json!({ "path": "assets/rules/a.rules.ron", "create": true, "skeleton": "tdm" }),
    );
    ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "session.objectives.capture_limit", "value": 7 }),
    );
    let switched = call(
        world,
        "editor.rules_open",
        json!({ "path": "assets/rules/b.rules.ron", "create": true, "skeleton": "tdm" }),
    );
    match &switched {
        Err((code, _)) if code == "match_rules_dirty" => {}
        other => gaps.push(format!(
            "rules_open of b while a is dirty (capture_limit 7) must be match_rules_dirty, got {other:?}"
        )),
    }
    if switched.is_ok() {
        assert!(
            undo_world(world),
            "the capture_limit edit is still on the undo stack"
        );
        let after = ok(world, "editor.match_rules_state", json!({}));
        let id = after["rules"]["id"].as_str().unwrap_or("");
        let limit = rules_field(&after, "session.objectives.capture_limit").and_then(Value::as_u64);
        if id != "a" || limit != Some(7) {
            gaps.push(format!(
                "undo after the document switch left id={id} capture_limit={limit:?} (want a and 7)"
            ));
        }
    }

    ok(
        world,
        "editor.playlist_edit",
        json!({
            "op": "open",
            "path": "assets/rot-a.playlist.ron",
            "create": true
        }),
    );
    ok(
        world,
        "editor.playlist_edit",
        json!({
            "op": "add",
            "row": { "map_id": "levels/arena.level.ron", "mode": "tdm" }
        }),
    );
    let playlist = call(
        world,
        "editor.playlist_edit",
        json!({
            "op": "open",
            "path": "assets/rot-b.playlist.ron",
            "create": true
        }),
    );
    match playlist {
        Err((code, _)) if code == "match_rules_dirty" => {}
        other => gaps.push(format!(
            "playlist open of rot-b while rot-a is dirty must be match_rules_dirty, got {other:?}"
        )),
    }

    assert!(gaps.is_empty(), "{gaps:#?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Start refuses a playlist row whose option is outside the rule set, with that row's `ruleset_*` code.
#[test]
fn review_start_refuses_a_playlist_row_outside_its_schema() {
    let root = regia_test_utils::unique_temp_dir("review-bad-row");
    write(&root, "assets/rules/ctf_long.rules.ron", CTF_LONG);
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "open", "path": "assets/rotation.playlist.ron", "create": true }),
    );
    ok(
        world,
        "editor.playlist_edit",
        json!({
            "op": "add",
            "row": {
                "map_id": "levels/arena.level.ron",
                "mode": "ctf_long",
                "options": { "capture_limit": "99" }
            }
        }),
    );
    let before = ok(world, "editor.match_rules_state", json!({}));
    assert_eq!(
        before["playlist"]["can_save"], false,
        "99 is past max 10: {before}"
    );
    let problems = before["playlist"]["problems"].to_string();
    assert!(
        problems.contains("ruleset_option_range"),
        "the row is out of range: {problems}"
    );
    let mut slot = start_play(&mut app);
    let state = control(app.world_mut(), &mut slot, json!({ "action": "start" }));
    assert_eq!(
        state["last_control"]["ok"], false,
        "start must refuse capture_limit 99 (max 10); got {state}"
    );
    let code = state["last_control"]["code"].as_str().unwrap_or("");
    assert!(
        code.starts_with("ruleset_"),
        "the refusal keeps the ruleset code, got {code}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn heat_overlay(scores: &[i64]) -> Result<(), Box<dyn std::any::Any + Send>> {
    let mut world = World::new();
    world.insert_resource(EditorCamera::default());
    let mut state = MatchPlayState::default();
    state.live = Some(MatchLive {
        tick: 1,
        phase: "play",
        paused: false,
        between: false,
        over: false,
        mode: "tdm",
        team_scores: Vec::new(),
        objectives: Vec::new(),
        touch_radius: 1.0,
        touch_height: None,
        teams: Vec::new(),
        playlist: (0, 0),
        seated: Vec::new(),
        heat: scores
            .iter()
            .enumerate()
            .map(|(index, score)| SpawnHeat {
                position: Vec3::new(index as f32 * 4.0, 0.0, 0.0),
                score: *score,
                eligible: true,
            })
            .collect(),
    });
    world.insert_resource(state);
    let view = ViewCamera::of(ViewKind::Perspective, world.resource::<EditorCamera>());
    catch_unwind(AssertUnwindSafe(|| {
        let _shapes = overlays(&world, &view, Vec2::new(640.0, 480.0));
    }))
}

/// Spawn-heat tint draws when two eligible scores are `i64::MIN` and `0` (a saturating `Scored` los weight).
#[test]
fn review_spawn_heat_overlay_accepts_saturating_scores() {
    assert!(heat_overlay(&[0, 10]).is_ok(), "a normal score span draws");
    assert!(
        heat_overlay(&[i64::MIN, 0]).is_ok(),
        "heat tint panics on scores i64::MIN and 0 (hi - lo / score - lo overflow)"
    );
}

/// The playlist grid edits a row through `set_row` (map, mode and options), the same op as the agent.
#[test]
fn review_playlist_section_edits_rows_through_set_row() {
    let src = include_str!("../src/match_rules_panel.rs");
    let start = src.find("fn playlist_section(").expect("playlist_section");
    let rest = &src[start..];
    let end = rest[1..].find("\nfn ").map_or(rest.len(), |at| at + 1);
    let body = &rest[..end];
    assert!(
        body.contains("set_row"),
        "playlist_section renders row labels and never calls set_row"
    );
}

// rules-editor-authoring-fixes (ADR-8805): the paths around the review proofs.

/// `discard: true` replaces a dirty rule set or playlist; the refusal before it changed nothing.
/// A folder load error names the file project-relative.
#[test]
fn discard_opens_over_unsaved_edits_and_load_errors_name_the_file() {
    let root = regia_test_utils::unique_temp_dir("fixes-discard");
    write(&root, "assets/rules/evil.rules.ron", "this is not a rule set\n");
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    let state = ok(world, "editor.match_rules_state", json!({}));
    let errors = state["load_errors"].as_array().cloned().unwrap_or_default();
    assert_eq!(errors.len(), 1, "{state}");
    assert_eq!(errors[0]["file"], "assets/rules/evil.rules.ron");
    assert_eq!(errors[0]["code"], "ruleset_parse");
    let shown = draw(world, 2, regia_editor::match_rules_panel::match_rules_panel);
    assert!(
        shown
            .iter()
            .any(|t| t.contains("assets/rules/evil.rules.ron") && t.contains("ruleset_parse")),
        "the panel lists the file that did not load: {shown:?}"
    );

    ok(
        world,
        "editor.rules_open",
        json!({ "path": "assets/rules/a.rules.ron", "create": true, "skeleton": "tdm" }),
    );
    ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "session.objectives.capture_limit", "value": 7 }),
    );
    let (refused, _) = code(
        world,
        "editor.rules_open",
        json!({ "path": "assets/rules/b.rules.ron", "create": true }),
    );
    assert_eq!(refused, "match_rules_dirty");
    let kept = ok(world, "editor.match_rules_state", json!({}));
    assert_eq!(kept["rules"]["id"], "a", "the refused open changed nothing");
    assert_eq!(
        rules_field(&kept, "session.objectives.capture_limit").and_then(Value::as_u64),
        Some(7)
    );
    let shown = draw(world, 2, regia_editor::match_rules_panel::match_rules_panel);
    assert!(
        shown.iter().any(|t| t == "Discard and open"),
        "the panel offers the discard while the set is dirty: {shown:?}"
    );
    let opened = ok(
        world,
        "editor.rules_open",
        json!({ "path": "assets/rules/b.rules.ron", "create": true, "discard": true }),
    );
    assert_eq!(opened["rules"]["id"], "b");
    // The earlier edit stays on the stack but applies only to its own document.
    assert!(undo_world(world));
    let after = ok(world, "editor.match_rules_state", json!({}));
    assert_eq!(after["rules"]["id"], "b");

    ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "open", "path": "assets/rot-a.playlist.ron", "create": true }),
    );
    ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "add", "row": { "map_id": "levels/arena.level.ron", "mode": "tdm" } }),
    );
    let opened = ok(
        world,
        "editor.playlist_edit",
        json!({ "op": "open", "path": "assets/rot-b.playlist.ron", "create": true, "discard": true }),
    );
    assert_eq!(opened["playlist"]["path"], "assets/rot-b.playlist.ron");
    assert_eq!(opened["playlist"]["rows"], json!([]));
    let _ = std::fs::remove_dir_all(&root);
}

/// Edits undone back to the file's value leave a clean document, and its save keeps the bytes. A
/// real edit of an existing file rewrites it without the new-file banner, ending in one newline.
#[test]
fn undo_to_clean_keeps_bytes_and_a_resave_has_no_banner() {
    let root = regia_test_utils::unique_temp_dir("fixes-clean-save");
    write(&root, "assets/rules/plain.rules.ron", PLAIN_RULES);
    let mut app = edit_app(Some(&root));
    let world = app.world_mut();
    ok(
        world,
        "editor.rules_open",
        json!({ "path": "assets/rules/plain.rules.ron" }),
    );
    ok(
        world,
        "editor.rules_set_field",
        json!({ "field": "session.objectives.capture_limit", "value": 4 }),
    );
    assert!(undo_world(world));
    let state = ok(world, "editor.rules_save", json!({}));
    assert_eq!(state["rules"]["dirty"], false);
    assert_eq!(
        file_bytes(&root, "assets/rules/plain.rules.ron"),
        PLAIN_RULES.as_bytes()
    );

    assert!(redo_world(world));
    ok(world, "editor.rules_save", json!({}));
    let text = std::fs::read_to_string(root.join("assets/rules/plain.rules.ron")).expect("saved");
    assert!(!text.starts_with("//"), "no banner on a re-save: {text}");
    assert!(text.ends_with(")\n") && !text.ends_with("\n\n"), "{text:?}");
    let back = regia_framework::ruleset_file::parse_rules_ron(&text).expect("reads");
    assert_eq!(back.session.objectives.capture_limit, Some(4));

    // A new file gets the banner once.
    ok(
        world,
        "editor.rules_open",
        json!({ "path": "assets/rules/fresh.rules.ron", "create": true, "skeleton": "ffa" }),
    );
    ok(world, "editor.rules_save", json!({}));
    let fresh = std::fs::read_to_string(root.join("assets/rules/fresh.rules.ron")).expect("saved");
    assert!(fresh.starts_with("// A rule set"), "{fresh}");
    assert!(!fresh.ends_with("\n\n"), "{fresh:?}");
    let _ = std::fs::remove_dir_all(&root);
}
