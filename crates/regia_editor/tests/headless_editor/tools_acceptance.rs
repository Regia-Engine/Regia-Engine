// SPDX-License-Identifier: MPL-2.0
//! One project authored over the real editor socket, reopened by a fresh process and cooked.
//! Assertions compare content, never timing or golden pixels. Independent regressions below
//! keep a failure in one tool from hiding evidence for the others.

use super::*;
#[path = "tools_runtime.rs"]
mod runtime;
use regia_2d::tilemap::{MapLayer, Orientation, TileDef, TileFrame, TileLayer, TileMapAsset, TilesetDef};
use regia_test_utils::write_atomic;
use std::collections::BTreeMap;

const CHILD: &str = "8b6f2a11-0000-4000-8000-000000000002";
const LEVEL: &str = "assets/maps/tools.level.ron";
const VARIANT: &str = "assets/props/tall.variant.ron";
const SEQUENCE: &str = "assets/cine/intro.seq.ron";
const UI: &str = "assets/ui/hud.ui.ron";
const MAP: &str = "assets/maps/room.tilemap.ron";

fn write(f: &Fixture, path: &str, bytes: impl AsRef<[u8]>) {
    let path = f.0.join(path);
    std::fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
    write_atomic(&path, bytes.as_ref()).expect("atomic fixture write");
}

fn fixture() -> Fixture {
    let f = Fixture::new("tools-acceptance");
    write(
        &f,
        "assets/props/lamp.prefab.ron",
        r#"(format_version: 1, entities: [
        (guid: "8b6f2a11-0000-4000-8000-000000000001", name: "LampRoot",
         components: {"regia_scene::components::Name": "LampRoot", "regia_scene::transform::Transform": ()}),
        (guid: "8b6f2a11-0000-4000-8000-000000000002", name: "LampChild",
         parent: Some("8b6f2a11-0000-4000-8000-000000000001"),
         components: {"regia_scene::components::Name": "LampChild", "regia_scene::transform::Transform": (translation: (0.0, 1.0, 0.0))}),
    ])"#,
    );
    write(
        &f,
        SEQUENCE,
        r#"(version: 3, name: "tools-intro", fps: 30.0, tracks: [
        (name: "camera", target: "camera", property: "transform.position",
         keys: Vec3([(time: 0.0, value: (0.0, 2.0, 5.0)), (time: 2.0, value: (2.0, 2.0, 5.0))])),
        (name: "audio", keys: Audio([(sound: "chime", start: 0.0, end: 2.0, volume_db: -12.0)])),
    ])"#,
    );
    write(
        &f,
        UI,
        r#"(version: 6, root: (style: (direction: column), children: [
        (name: "title", text: (spans: [Loc("tools-title")]), style: (size: (300, 40))),
        (name: "score", text: (spans: ["0"]), style: (size: (300, 40))),
    ]))"#,
    );
    let samples: Vec<f32> = (0..24_000).map(|i| (i % 40) as f32 / 40.0 - 0.5).collect();
    write(
        &f,
        "assets/audio/chime.wav",
        regia_media::encode::wav::write_pcm16(8_000, 1, &samples),
    );
    write(&f, "assets/localization/en/main.ftl", "tools-title = Workshop\n");
    write(&f, "assets/localization/fr/main.ftl", "tools-title = Atelier\n");
    let map = TileMapAsset {
        version: regia_2d::tilemap::TILEMAP_FORMAT_VERSION,
        orientation: Orientation::Orthogonal,
        width: 6,
        height: 4,
        tile_width: 16,
        tile_height: 16,
        background: None,
        tilesets: vec![TilesetDef {
            name: "walls".into(),
            first_gid: 1,
            image: "tiles.png".into(),
            image_width: 64,
            image_height: 64,
            tile_width: 16,
            tile_height: 16,
            columns: 4,
            tile_count: 16,
            spacing: 0,
            margin: 0,
            tiles: vec![TileDef {
                id: 2,
                animation: vec![
                    TileFrame {
                        tile: 2,
                        duration_ms: 100,
                    },
                    TileFrame {
                        tile: 3,
                        duration_ms: 100,
                    },
                ],
                ..Default::default()
            }],
        }],
        layers: vec![MapLayer::Tiles(TileLayer {
            name: "walls".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0; 24],
            properties: BTreeMap::new(),
        })],
        properties: BTreeMap::new(),
    };
    write(&f, MAP, map.to_ron().expect("map RON"));
    write(
        &f,
        "assets/maps/walls.autotile.ron",
        r#"(version: 1, tileset: "walls", border: Empty,
        terrains: [(name: "wall", layout: Edge16([0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]))])"#,
    );
    let mut pixels = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut pixels, 64, 64);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("PNG header")
            .write_image_data(&[64, 128, 192, 255].repeat(64 * 64))
            .expect("PNG pixels");
    }
    write(&f, "assets/maps/tiles.png", pixels);
    f
}

fn start(f: &Fixture) -> (Editor, Client) {
    let editor = if std::env::var_os("REGIA_TOOLS_GPU").is_some() {
        Editor::start_with(f, &[], &[])
    } else {
        Editor::start(f)
    };
    editor.event("ready", f);
    let client = connect(f);
    (editor, client)
}

fn stop(editor: &mut Editor, client: &mut Client, f: &Fixture) {
    let out = call(client, "agent.shutdown", json!({}));
    assert_eq!(out["exiting"], true, "{out}");
    editor.event("exit", f);
    assert_eq!(editor.wait().code(), Some(0), "{}", f.log());
}

fn expect_position(client: &mut Client, name: &str, axis: &str, value: f32) {
    let condition = json!({"entity": {"name": name}, "component": "Transform",
        "path": format!("translation.{axis}"), "op": "eq", "value": value});
    let waited = call(
        client,
        "play.wait_until",
        json!({"condition": condition, "timeout_ms": 5000}),
    );
    if waited["satisfied"] != true {
        let entities = call(
            client,
            "scene.query_entities",
            json!({"limit": 100, "component": "Transform"}),
        );
        panic!("{name}: {waited}; live entities: {entities}");
    }
    let checked = call(
        client,
        "play.expect",
        json!({"assertions": [{"name": name, "condition": condition}]}),
    );
    assert_eq!(checked["pass"], true, "{checked}");
}

fn create_variant(client: &mut Client) {
    call(
        client,
        "editor.create_prefab_variant",
        json!({"base": "assets/props/lamp.prefab.ron", "path": VARIANT}),
    );
    call(
        client,
        "editor.set_variant_override",
        json!({"path": VARIANT, "target": CHILD,
        "component": "regia_scene::transform::Transform", "field": "translation.y", "value": 3.0}),
    );
}

fn map_data(f: &Fixture) -> TileMapAsset {
    TileMapAsset::from_ron(&level_text(f, MAP)).expect("saved map")
}

fn ui_content(client: &mut Client) -> Value {
    let state = call(client, "editor.ui_designer_state", json!({"node": "#score"}));
    json!({"fields": state["fields"], "tree": state["tree"]})
}

fn preview(client: &mut Client, locale: &str, pseudo: bool) -> Value {
    call(
        client,
        "editor.ui_designer_preview",
        json!({"size": [640, 480], "locale": locale,
        "pseudo": pseudo, "samples": {"Score.value": "7"}}),
    )
}

fn node_text(preview: &Value, name: &str) -> Value {
    preview["nodes"]
        .as_array()
        .expect("preview nodes")
        .iter()
        .find(|node| node["name"] == name)
        .expect("named preview node")["text"]
        .clone()
}

#[test]
fn tools_author_save_reopen_undo_cook() {
    let f = fixture();
    terrain_fixture(&f);
    let (mut editor, mut client) = start(&f);
    let terrain_after = author_terrain(&mut client, &f);
    println!("tools: author prefab");
    create_variant(&mut client);
    let variant = call(&mut client, "editor.prefab_variant", json!({"path": VARIANT}));
    assert_eq!(variant["format_version"], 1);
    assert_eq!(variant["chain"].as_array().expect("variant chain").len(), 2);
    let instance = call(&mut client, "editor.spawn_prefab", json!({"path": VARIANT}));
    assert!(instance["entity"].is_u64(), "{instance}");
    expect_position(&mut client, "LampChild", "y", 3.0);
    let camera = call(
        &mut client,
        "editor.spawn",
        json!({"name": "CameraTrack", "translation": [0, 2, 5]}),
    );
    call(
        &mut client,
        "editor.add_component",
        json!({"entity": camera["entity"], "component": "Camera"}),
    );
    expect_position(&mut client, "CameraTrack", "y", 2.0);

    // Neighbor-dependent auto-tiling, including an animated cap, uses the scene undo stack.
    let original_map = map_data(&f);
    call(
        &mut client,
        "editor.mode2d_set",
        json!({"active": true, "map": MAP,
        "rules": "assets/maps/walls.autotile.ron", "layer": "walls", "brush": {"terrain": "wall"}}),
    );
    let painted = call(
        &mut client,
        "editor.tile_stroke",
        json!({"cells": [[0,1],[1,1],[2,1],[3,1],[4,1]]}),
    );
    assert_eq!(painted["changed"], 5, "{painted}");
    call(&mut client, "editor.tilemap_save", json!({}));
    let authored_map = map_data(&f);
    assert_eq!(
        &authored_map.tile_layer(0).expect("layer").data[6..12],
        &[3, 11, 11, 11, 9, 0]
    );
    call(&mut client, "editor.undo", json!({}));
    call(&mut client, "editor.tilemap_save", json!({}));
    assert_eq!(map_data(&f), original_map);
    call(&mut client, "editor.redo", json!({}));
    call(&mut client, "editor.tilemap_save", json!({}));
    assert_eq!(map_data(&f), authored_map);

    let ui_after = author_ui(&mut client);

    let (curve_after, audio_after) = author_sequence(&mut client);
    println!("tools: save and reopen combined scene");
    call(&mut client, "editor.save_level", json!({"path": LEVEL}));
    for (path, version) in [
        (LEVEL, regia_scene::format::SCENE_FORMAT_VERSION),
        (VARIANT, 1),
        (UI, regia_ui::authoring::UI_RON_VERSION),
        (SEQUENCE, regia_framework::sequencer::SEQUENCE_FORMAT_VERSION),
        (MAP, regia_2d::tilemap::TILEMAP_FORMAT_VERSION),
    ] {
        assert_written_version(&f, path, version);
    }
    stop(&mut editor, &mut client, &f);

    let (mut editor, mut client) = start(&f);
    call(&mut client, "editor.open_level", json!({"path": LEVEL}));
    assert_eq!(
        export_heights(&mut client, &f, "combined-reopened.r16"),
        terrain_after
    );
    expect_position(&mut client, "LampChild", "y", 3.0);
    expect_position(&mut client, "CameraTrack", "y", 2.0);
    call(&mut client, "editor.ui_designer_open", json!({"path": UI}));
    assert_eq!(ui_content(&mut client), ui_after);
    assert_eq!(node_text(&preview(&mut client, "fr", false), "title"), "Atelier");
    assert_eq!(node_text(&preview(&mut client, "en", false), "score"), "007");
    call(&mut client, "editor.mode2d_set", json!({"map": MAP}));
    assert_eq!(map_data(&f), authored_map);
    let reopened = call(&mut client, "editor.seq_curve_open", json!({"path": SEQUENCE}));
    let player = reopened["player"].clone();
    assert_eq!(
        call(
            &mut client,
            "editor.seq_curve_state",
            json!({"player": player, "track": "camera", "component": 0})
        )["keys"],
        curve_after
    );
    assert_eq!(
        call(
            &mut client,
            "editor.seq_sections_state",
            json!({"player": player, "track": "audio"})
        )["sections"],
        audio_after
    );
    call(
        &mut client,
        "sequence.player",
        json!({"action": "play", "player": player,
        "clock": "external", "bindings": {"camera": {"name": "CameraTrack"}}}),
    );
    call(
        &mut client,
        "sequence.player",
        json!({"action": "seek", "player": player, "time": 1.0}),
    );
    expect_position(&mut client, "CameraTrack", "x", 2.0);
    let sampled = call(
        &mut client,
        "sequence.player",
        json!({"action": "sample", "player": player, "time": 1.0}),
    );
    assert_eq!(sampled["sample"]["sounds"][0]["sound"], "chime", "{sampled}");
    call(
        &mut client,
        "sequence.player",
        json!({"action": "stop", "player": player}),
    );
    call(&mut client, "editor.play", json!({}));
    let playing = wait_pie(&mut client, "playing", 0);
    assert!(playing["entities"].as_u64().unwrap_or(0) >= 3, "{playing}");
    call(&mut client, "editor.pause", json!({}));
    let paused = wait_pie(&mut client, "paused", 0);
    let frames = paused["frames"].as_u64().expect("play frame counter");
    call(&mut client, "editor.step", json!({"frames": 2}));
    let stepped = wait_pie(&mut client, "paused", frames + 2);
    assert_eq!(stepped["frames"], frames + 2);
    call(&mut client, "editor.stop", json!({}));
    wait_pie(&mut client, "stopped", 0);
    expect_position(&mut client, "LampChild", "y", 3.0);
    expect_position(&mut client, "CameraTrack", "y", 2.0);
    if std::env::var_os("REGIA_TOOLS_GPU").is_some() {
        let ui_capture = call(
            &mut client,
            "editor.ui_designer_render",
            json!({"size": [640,480],
            "locale": "fr", "pseudo": false, "samples": {"Score.value": "7"}}),
        );
        assert!(
            ui_capture["drawn_pixels"].as_u64().unwrap_or(0) > 0,
            "{ui_capture}"
        );
        retain_evidence(
            "tools-ui.json",
            &serde_json::to_vec_pretty(&ui_capture).expect("UI capture"),
        );
        let capture = call(
            &mut client,
            "editor.capture_viewport",
            json!({"width": 320,
            "height": 240, "view": "perspective", "quality": "desktop_low", "warm_frames": 2,
            "timeout_ms": 300_000}),
        );
        assert!(!capture["png_base64"].as_str().expect("viewport PNG").is_empty());
        retain_evidence(
            "tools-viewport.json",
            &serde_json::to_vec_pretty(&capture).expect("capture JSON"),
        );
    }
    stop(&mut editor, &mut client, &f);
    let cooked = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(f.0.clone()))
        .expect("cook project");
    assert_cook_success(&cooked);
    let cache = regia_asset::cooked_cache_dir(&f.0, regia_asset_pipeline::host_platform());
    let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
        .expect("manifest read")
        .expect("manifest exists");
    let packs = regia_asset_pipeline::pak_writer::pack_cooked_directory(
        &cache,
        &f.0.join("content"),
        &BTreeMap::new(),
        true,
    )
    .expect("pack");
    assert_eq!(packs.len(), 1);
    let pak = regia_asset::PakReader::open(&packs[0]).expect("runtime pak reader");
    let path = MAP;
    let entry = manifest
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("{path} missing from manifest"));
    let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(entry.id))).expect("cooked asset");
    let packaged = pak.read_decompressed(entry.id).expect("runtime package asset");
    assert_eq!(packaged, bytes, "{path}");
    let (_, payload) = regia_asset::decode_cooked_file(&packaged).expect("cooked header");
    let runtime = regia_2d::tilemap::CookedTileMap::from_postcard(payload).expect("runtime tilemap");
    assert_eq!(runtime.map, authored_map);
    for (time, tile) in [(0, 2), (99, 2), (100, 3), (199, 3), (200, 2)] {
        assert_eq!(
            regia_2d::batch::animated_tile(&runtime.map.tilesets[0], 2, time),
            tile
        );
    }
    runtime::verify(&f.0).expect("runtime consumers match authored output");
    export_runtime_project(&f);
}

#[test]
fn tools_prefab_variant_override_is_undoable() {
    let f = fixture();
    let (mut editor, mut client) = start(&f);
    create_variant(&mut client);
    call(&mut client, "editor.spawn_prefab", json!({"path": VARIANT}));
    expect_position(&mut client, "LampChild", "y", 3.0);
    let derived = "assets/props/derived.variant.ron";
    call(
        &mut client,
        "editor.create_prefab_variant",
        json!({"base": VARIANT, "path": derived}),
    );
    call(
        &mut client,
        "editor.set_variant_override",
        json!({"path": derived, "target": CHILD,
        "component": "regia_scene::components::Name", "field": ".", "value": "\"DerivedChild\""}),
    );
    assert_eq!(
        call(&mut client, "editor.prefab_variant", json!({"path": derived}))["chain"]
            .as_array()
            .expect("chain")
            .len(),
        3
    );
    call(&mut client, "editor.spawn_prefab", json!({"path": derived}));
    expect_position(&mut client, "DerivedChild", "y", 3.0);
    let before = std::fs::read(f.0.join(VARIANT)).expect("variant bytes");
    let depth = call(&mut client, "editor.history", json!({}))["undo_depth"]
        .as_u64()
        .expect("depth");
    call(
        &mut client,
        "editor.set_variant_override",
        json!({"path": VARIANT, "target": CHILD,
        "component": "regia_scene::transform::Transform", "field": "translation.y", "value": 5.0}),
    );
    let after = std::fs::read(f.0.join(VARIANT)).expect("variant bytes");
    assert_ne!(before, after);
    expect_position(&mut client, "LampChild", "y", 5.0);
    expect_position(&mut client, "DerivedChild", "y", 5.0);
    assert_eq!(
        call(&mut client, "editor.history", json!({}))["undo_depth"],
        depth + 1,
        "a saved variant override must be one undo entry"
    );
    call(&mut client, "editor.undo", json!({}));
    assert_eq!(std::fs::read(f.0.join(VARIANT)).expect("undone bytes"), before);
    expect_position(&mut client, "LampChild", "y", 3.0);
    expect_position(&mut client, "DerivedChild", "y", 3.0);
    call(&mut client, "editor.redo", json!({}));
    assert_eq!(std::fs::read(f.0.join(VARIANT)).expect("redone bytes"), after);
    expect_position(&mut client, "LampChild", "y", 5.0);
    expect_position(&mut client, "DerivedChild", "y", 5.0);
    call(
        &mut client,
        "editor.set_variant_override",
        json!({"path": VARIANT, "target": CHILD,
        "component": "regia_scene::transform::Transform", "field": "translation.y", "value": 5.0}),
    );
    assert_eq!(
        call(&mut client, "editor.history", json!({}))["undo_depth"],
        depth + 1,
        "an identical override must not create history"
    );
    stop(&mut editor, &mut client, &f);
}

#[test]
fn tools_editor_has_a_real_flame_capture() {
    let f = fixture();
    let (mut editor, mut client) = start(&f);
    let enabled = call(&mut client, "profiler.set_capture", json!({"flame": true}));
    assert_eq!(enabled["flame"], true);
    for i in 0..8 {
        call(
            &mut client,
            "editor.spawn",
            json!({"name": format!("Capture{i}")}),
        );
    }
    let captured = call(
        &mut client,
        "profiler.capture",
        json!({"path": "captures/tools.rprof.json", "frames": 8}),
    );
    assert!(captured["nodes"].as_u64().unwrap_or(0) > 0, "{captured}");
    let file: Value =
        serde_json::from_str(&level_text(&f, "captures/tools.rprof.json")).expect("capture JSON");
    // The current profile-capture version (v2 since ADR-7325).
    assert_eq!(
        file["format_version"],
        regia::telemetry::profile_capture::PROFILE_CAPTURE_FORMAT_VERSION
    );
    assert!(!file["cpu"].as_array().expect("flame roots").is_empty());
    assert!(!file["folded"].as_str().expect("folded stacks").is_empty());
    retain_evidence(
        "tools.rprof.json",
        &serde_json::to_vec_pretty(&file).expect("capture JSON"),
    );
    stop(&mut editor, &mut client, &f);
}

fn terrain_fixture(f: &Fixture) {
    write(
        f,
        "assets/maps/ground/terrain.terrain.layers.ron",
        include_str!("../../../regia_terrain/tests/fixtures/ground.terrain.layers.ron"),
    );
    write(
        f,
        "assets/maps/ground/heightfield.json",
        r#"{"size_m":64.0,"origin":[0.0,0.0,0.0]}"#,
    );
    let mut bytes = Vec::new();
    {
        let mut png = png::Encoder::new(&mut bytes, 65, 65);
        png.set_color(png::ColorType::Grayscale);
        png.set_depth(png::BitDepth::Sixteen);
        png.write_header()
            .expect("height PNG header")
            .write_image_data(&16384_u16.to_be_bytes().repeat(65 * 65))
            .expect("height PNG");
    }
    write(f, "assets/maps/ground/heightfield.png", bytes);
    let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(f.0.clone()))
        .expect("initial terrain cook");
    assert_cook_success(&report);
}

fn export_heights(client: &mut Client, f: &Fixture, name: &str) -> Vec<u8> {
    let out = client
        .call("terrain.export_heights", json!({"path": name}))
        .unwrap_or_else(|error| panic!("terrain export {name}: {error}; editor log: {}", f.log()));
    checked_export(&out, f, name)
}

fn checked_export(out: &Value, f: &Fixture, name: &str) -> Vec<u8> {
    assert_eq!(out["clamped_low"], 0, "{out}");
    assert_eq!(out["clamped_high"], 0, "{out}");
    std::fs::read(f.0.join(name)).expect("exported heightfield")
}

/// What `terrain.export_heights` answers while the open level's terrain is not spawned yet.
const NO_TERRAIN: &str = "no Terrain in this world";

/// [`export_heights`] right after `editor.open_level`. The level's terrain is spawned by an
/// `Update` system once the opened level's settings land, so under heavy host load the first
/// replies after the open can still find no terrain (sprint-rev-claude-26-k finding 2). This
/// retries only that one answer, a bounded number of times; each reply crosses an editor frame
/// boundary (no sleep, no wall-clock budget). Every other error fails at once, and the export it
/// finally gets is checked exactly like [`export_heights`].
fn export_heights_after_open(client: &mut Client, f: &Fixture, name: &str) -> Vec<u8> {
    for _ in 0..1_000 {
        match client.call("terrain.export_heights", json!({"path": name})) {
            Ok(out) => return checked_export(&out, f, name),
            Err(ClientError::Rpc(error)) if error.message == NO_TERRAIN => std::thread::yield_now(),
            Err(error) => panic!("terrain export {name}: {error}; editor log: {}", f.log()),
        }
    }
    panic!(
        "terrain export {name}: the opened level's terrain never spawned; editor log: {}",
        f.log()
    );
}

#[test]
fn tools_terrain_sculpt_undo_reopen_and_runtime_data() {
    let f = fixture();
    terrain_fixture(&f);
    let (mut editor, mut client) = start(&f);
    let after = author_terrain(&mut client, &f);
    stop(&mut editor, &mut client, &f);
    let (mut editor, mut client) = start(&f);
    call(&mut client, "editor.open_level", json!({"path": LEVEL}));
    assert_eq!(export_heights_after_open(&mut client, &f, "reopened.r16"), after);
    stop(&mut editor, &mut client, &f);
    for platform in ["linux", "windows"] {
        assert_terrain_runtime(&f, &after, platform);
    }
}

/// Keep the exact authored inputs for a subsequent real CLI package/run, when requested by the runner.
fn export_runtime_project(f: &Fixture) {
    let Some(destination) = std::env::var_os("REGIA_TOOLS_EVIDENCE_DIR") else {
        return;
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory");
    let mut manifest = String::from(
        "[package]\nname = \"tools-runtime\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n[dependencies]\n",
    );
    for name in [
        "regia_app",
        "regia_ecs",
        "regia_scene",
        "regia_asset",
        "regia_2d",
        "regia_ui",
        "regia_framework",
        "regia_terrain",
        "regia_render",
        "regia_audio",
    ] {
        let path = serde_json::to_string(&root.join(name).display().to_string()).expect("path JSON");
        manifest.push_str(&format!("{name} = {{ path = {path} }}\n"));
    }
    manifest.push_str("[profile.dev]\nopt-level = 1\ndebug = \"line-tables-only\"\n[profile.dev.package.\"*\"]\nopt-level = 3\n");
    // Match the runtime dependency closure, not every crate in the editor workspace.
    // Unused package overrides otherwise produce Cargo warnings in the exported project.
    for name in [
        "regia_2d",
        "regia_animation",
        "regia_app",
        "regia_asset",
        "regia_audio",
        "regia_core",
        "regia_ecs",
        "regia_framework",
        "regia_input",
        "regia_macros",
        "regia_nav",
        "regia_physics",
        "regia_platform",
        "regia_render",
        "regia_scene",
        "regia_terrain",
        "regia_ui",
        "regia_vfx",
    ] {
        manifest.push_str(&format!("[profile.dev.package.{name}]\nopt-level = 1\n"));
    }
    write(f, "Cargo.toml", manifest);
    let lock = std::fs::read(root.parent().expect("workspace root").join("Cargo.lock"))
        .expect("audited dependency lock");
    write(f, "Cargo.lock", lock);
    write(f, "src/main.rs", include_str!("tools_runtime.rs"));
    let destination = PathBuf::from(destination).join("tools-project");
    std::fs::create_dir_all(destination.parent().expect("evidence parent")).expect("evidence directory");
    assert!(!destination.exists(), "refuse to replace prior evidence");
    std::fs::rename(&f.0, &destination).expect("retain authored project");
    println!("tools_project={}", destination.display());
}

fn author_terrain(client: &mut Client, f: &Fixture) -> Vec<u8> {
    call(
        client,
        "editor.spawn",
        json!({"name": "TerrainAnchor", "translation": [0, 1, 0]}),
    );
    call(
        client,
        "terrain.set_level_terrain",
        json!({"terrain": {
        "layers": "maps/ground/terrain.terrain.layers.ron", "world_width_m": 64.0,
        "height_scale_m": 40.0, "height_offset_m": -10.0}}),
    );
    call(client, "editor.save_level", json!({"path": LEVEL}));
    call(client, "editor.open_level", json!({"path": LEVEL}));
    expect_position(client, "TerrainAnchor", "y", 1.0);
    // The same open-then-export race as the reopen below.
    let before = export_heights_after_open(client, f, "before.r16");
    let stroke = call(
        client,
        "terrain.stroke",
        json!({"mode": "raise", "center": [0,0],
        "radius": 4.0, "strength": 2.0, "hardness": 1.0}),
    );
    assert!(stroke["samples"].as_u64().unwrap_or(0) > 0, "{stroke}");
    let after = export_heights(client, f, "after.r16");
    assert_ne!(after, before);
    assert_eq!(call(client, "editor.undo", json!({}))["undone"], true);
    assert_eq!(export_heights(client, f, "undo.r16"), before);
    assert_eq!(call(client, "editor.redo", json!({}))["redone"], true);
    assert_eq!(export_heights(client, f, "redo.r16"), after);
    let saved = call(client, "editor.save_level", json!({}));
    assert!(
        saved["terrain_tiles_recooked"].as_u64().unwrap_or(0) > 0,
        "{saved}"
    );
    // Preserve authored heights in their source too, so a clean platform cook cannot discard them.
    export_heights(client, f, "assets/maps/ground/heightfield.png");
    write(f, "assets/terrain-expected.r16", &after);
    after
}

fn retain_evidence(name: &str, bytes: &[u8]) {
    if let Some(directory) = std::env::var_os("REGIA_TOOLS_EVIDENCE_DIR") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("evidence directory");
        write_atomic(&directory.join(name), bytes).expect("atomic evidence write");
    }
}

fn author_ui(client: &mut Client) -> Value {
    // Designer history is independent of the scene history. Compare the entire authored node/tree.
    call(client, "editor.ui_designer_open", json!({"path": UI}));
    let ui_before = ui_content(client);
    call(
        client,
        "editor.ui_designer_edit",
        json!({"op": "set", "node": "#score", "key": "bindings",
        "value": [{"source": "Score.value", "target": "text", "format": "{:03}"}]}),
    );
    let ui_after = ui_content(client);
    assert_ne!(ui_after, ui_before);
    call(client, "editor.ui_designer_edit", json!({"op": "undo"}));
    assert_eq!(ui_content(client), ui_before);
    call(client, "editor.ui_designer_edit", json!({"op": "redo"}));
    assert_eq!(ui_content(client), ui_after);
    let english = preview(client, "en", false);
    assert_eq!(node_text(&english, "title"), "Workshop");
    assert_eq!(node_text(&english, "score"), "007");
    assert_eq!(node_text(&preview(client, "fr", false), "title"), "Atelier");
    assert_ne!(node_text(&preview(client, "fr", true), "title"), "Atelier");
    call(client, "editor.ui_designer_save", json!({}));

    ui_after
}

fn author_sequence(client: &mut Client) -> (Value, Value) {
    // The camera curve and audio section both undo through the scene history.
    let opened = call(client, "editor.seq_curve_open", json!({"path": SEQUENCE}));
    let player = opened["player"].clone();
    let curve_before = call(
        client,
        "editor.seq_curve_state",
        json!({"player": player, "track": "camera", "component": 0}),
    )["keys"]
        .clone();
    call(
        client,
        "editor.seq_curve_edit",
        json!({"player": player, "track": "camera", "component": 0,
        "op": "move", "index": 1, "time": 2.0, "value": 4.0}),
    );
    let curve_after = call(client, "editor.seq_curve_state", json!({}))["keys"].clone();
    assert_ne!(curve_after, curve_before);
    call(client, "editor.undo", json!({}));
    assert_eq!(
        call(client, "editor.seq_curve_state", json!({}))["keys"],
        curve_before
    );
    call(client, "editor.redo", json!({}));
    assert_eq!(
        call(client, "editor.seq_curve_state", json!({}))["keys"],
        curve_after
    );
    let audio_before = call(
        client,
        "editor.seq_sections_state",
        json!({"player": player, "track": "audio"}),
    )["sections"]
        .clone();
    call(
        client,
        "editor.seq_section_edit",
        json!({"player": player, "track": "audio", "op": "gain", "index": 0, "value": -6.0}),
    );
    let audio_after = call(client, "editor.seq_sections_state", json!({}))["sections"].clone();
    assert_ne!(audio_after, audio_before);
    call(client, "editor.undo", json!({}));
    assert_eq!(
        call(client, "editor.seq_sections_state", json!({}))["sections"],
        audio_before
    );
    call(client, "editor.redo", json!({}));
    assert_eq!(
        call(client, "editor.seq_sections_state", json!({}))["sections"],
        audio_after
    );
    let saved = call(client, "editor.seq_curve_save", json!({"player": player}));
    // The editor saves at the current `.seq.ron` version (v5 since ADR-2948; this said 3).
    assert_eq!(
        saved["version"],
        regia_framework::sequencer::SEQUENCE_FORMAT_VERSION
    );
    (curve_after, audio_after)
}

#[test]
fn tools_designer_binding_localization_undo_reopen() {
    let f = fixture();
    let (mut editor, mut client) = start(&f);
    let authored = author_ui(&mut client);
    stop(&mut editor, &mut client, &f);
    let (mut editor, mut client) = start(&f);
    call(&mut client, "editor.ui_designer_open", json!({"path": UI}));
    assert_eq!(ui_content(&mut client), authored);
    assert_eq!(node_text(&preview(&mut client, "fr", false), "title"), "Atelier");
    assert_eq!(node_text(&preview(&mut client, "en", false), "score"), "007");
    stop(&mut editor, &mut client, &f);
}

#[test]
fn tools_sequence_camera_audio_undo_reopen() {
    let f = fixture();
    let (mut editor, mut client) = start(&f);
    let (curve, audio) = author_sequence(&mut client);
    call(&mut client, "editor.save_level", json!({"path": LEVEL}));
    stop(&mut editor, &mut client, &f);
    let (mut editor, mut client) = start(&f);
    call(&mut client, "editor.seq_curve_open", json!({"path": SEQUENCE}));
    assert_eq!(
        call(
            &mut client,
            "editor.seq_curve_state",
            json!({"track": "camera", "component": 0})
        )["keys"],
        curve
    );
    assert_eq!(
        call(
            &mut client,
            "editor.seq_sections_state",
            json!({"track": "audio"})
        )["sections"],
        audio
    );
    stop(&mut editor, &mut client, &f);
}

fn assert_cook_success(report: &regia_asset_pipeline::CookReport) {
    // The CLI deliberately treats these as raw packaged files, not cookable pak payloads.
    // Limit the benign diagnostics to this fixture's known raw inputs; all other errors fail.
    for (path, error) in &report.errors {
        assert!(
            [
                LEVEL,
                VARIANT,
                SEQUENCE,
                UI,
                "assets/props/lamp.prefab.ron",
                "assets/localization/en/main.ftl",
                "assets/localization/fr/main.ftl",
                "assets/terrain-expected.r16",
                "assets/maps/ground/heightfield.json",
            ]
            .contains(&path.as_str()),
            "unexpected cook diagnostic: {path}: {error}"
        );
        let extension = std::path::Path::new(path)
            .extension()
            .and_then(|v| v.to_str())
            .expect("raw extension");
        assert_eq!(
            error,
            &format!("unsupported source extension: {extension}"),
            "{path}"
        );
    }
}

#[test]
fn tools_android_authored_level_terrain_runtime_data() {
    let f = fixture();
    terrain_fixture(&f);
    let (mut editor, mut client) = start(&f);
    let after = author_terrain(&mut client, &f);
    stop(&mut editor, &mut client, &f);
    assert_terrain_runtime(&f, &after, "android");
}

fn assert_terrain_runtime(f: &Fixture, after: &[u8], platform: &str) {
    let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
        project_root: f.0.clone(),
        platform: platform.into(),
        incremental: false,
    })
    .expect("platform terrain cook");
    assert_cook_success(&report);
    let cache = regia_asset::cooked_cache_dir(&f.0, platform);
    let level = regia_scene::level::Level::from_ron_str(
        regia_scene::level::LevelId::default(),
        &level_text(f, LEVEL),
    )
    .expect("level consumer");
    let loaded = regia_terrain::load_level_terrain(
        &f.0.join("assets"),
        &cache,
        "maps/tools.level.ron",
        level.settings.terrain.as_ref(),
    )
    .expect("runtime terrain consumer");
    let terrain = loaded.terrain.expect("named terrain present");
    assert_eq!(
        regia_terrain::height_io::encode_r16(
            &terrain.map.to_heightfield(),
            regia_terrain::height_io::RawEndian::Little
        ),
        after,
        "{platform}"
    );
}

fn assert_written_version(f: &Fixture, path: &str, expected: u32) {
    #[derive(serde::Deserialize)]
    struct Revision {
        #[serde(alias = "format_version")]
        version: u32,
    }
    // Read the on-disk header directly; a migrating runtime loader would hide an old writer.
    let text = level_text(f, path);
    let text = text.trim_start();
    let opening = text.find('(').expect("written struct body");
    assert!(
        text[..opening]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "{path}: struct prefix"
    );
    let revision: Revision = ron::from_str(&text[opening..]).expect("written format header");
    assert_eq!(revision.version, expected, "{path}");
}

fn wait_pie(client: &mut Client, mode: &str, minimum_frames: u64) -> Value {
    // Each reply crosses an editor frame boundary; no sleep or elapsed-time assertion.
    for _ in 0..1_000 {
        let state = call(client, "editor.play_state", json!({}));
        assert!(state["panic"].is_null(), "{state}");
        if state["mode"] == mode && state["frames"].as_u64().unwrap_or(0) >= minimum_frames {
            return state;
        }
        std::thread::yield_now();
    }
    panic!("PIE never reached {mode} at frame {minimum_frames}");
}
