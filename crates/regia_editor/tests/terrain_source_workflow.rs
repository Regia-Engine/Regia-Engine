// SPDX-License-Identifier: MPL-2.0
//! Terrain ▸ Import source over the agent protocol (ADR-8541, CT-20): `editor.terrain_source_*`
//! through the real dispatcher, `play.wait_until` on the workflow's agent events and
//! `play.expect` on the result.
//!
//! What each test proves:
//!
//! - the CLI's service (`regia_agent::read_project_source` + `source_summary`, what `regia terrain
//!   source-inspect` prints), the agent's `terrain.inspect_source` and the panel's preview agree on
//!   bounds and datum;
//! - a destination mapping that differs needs an explicit conversion before apply;
//! - a cancel before apply (while the worker runs, or once staged) changes no file and no height;
//! - a source edited during the preview refuses the stale apply (and so does a terrain edited);
//! - a small import is one undo step and undo restores the heights bit for bit;
//! - a large import cannot silently drop recoverability: it needs acknowledgement and writes a
//!   backup that restores the original bits;
//! - a reimport repeats the recorded source and settings, and the save re-cooks only the tiles
//!   it changed.
//!
//! The worker is synchronised by a channel gate and by joining it
//! (`terrain_source_import::wait_worker`), never by sleeps or wall-clock budgets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, Response, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use regia_editor::terrain_source_import as tsi;
use regia_terrain::{CdlodConfig, Terrain, TerrainLayers, TerrainMap};
use serde_json::{Value, json};

/// Samples per side of the destination terrain.
const SIDE: u32 = 65;
/// Metres per count of the fixtures' sources.
const MPC: f32 = 0.01;
/// The fixtures' datum: count 500 is 0 m, so count 0 is the terrain's lowest height (-5 m).
const ZERO: u16 = 500;

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
    root: PathBuf,
}

impl Client {
    fn new(name: &str) -> Self {
        let root = regia_test_utils::unique_temp_dir(name);
        std::fs::create_dir_all(root.join("assets/maps")).expect("assets");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
        app.insert_resource(
            regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            })
            .expect("asset server"),
        );
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<Selection>()
            .init_resource::<regia_agent::AgentEventLog>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        app.add_systems(Last, regia_editor::editor_jobs::tick_system);
        app.world_mut()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(root.clone()),
            });
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
            root,
        }
    }

    fn world(&mut self) -> &mut World {
        self.app.world_mut()
    }

    fn send(&mut self, method: &str, params: Value) -> crossbeam_channel::Receiver<Response> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        answer
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let answer = self.send(method, params);
        self.app.update();
        let response = answer
            .try_recv()
            .expect("the dispatcher answers within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            (None, None) => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params.clone()) {
            Ok(value) => value,
            Err(err) => panic!("{method} {params}: {err:?}"),
        }
    }

    /// `play.wait_until` on the workflow event `<job> <state>`: the request is sent, the worker is
    /// joined (it has returned when this returns), then a bounded number of frames applies its
    /// end and answers the wait.
    fn wait_for(&mut self, job: &str, state: &str) -> Value {
        let answer = self.send(
            "play.wait_until",
            json!({
                "condition": { "events_contain": { "kind": tsi::EVENT_KIND, "contains": format!("{job} {state}") } },
                "timeout_ms": 60_000
            }),
        );
        tsi::wait_worker(self.world());
        for _ in 0..8 {
            self.app.update();
            if let Ok(response) = answer.try_recv() {
                return response
                    .result
                    .unwrap_or_else(|| panic!("wait for {job} {state}: {:?}", response.error));
            }
        }
        panic!("{job} never reached {state} within 8 frames after its worker returned");
    }

    /// `play.expect` on the workflow event: the assertion holds.
    fn expect_event(&mut self, job: &str, state: &str) {
        let report = self.call(
            "play.expect",
            json!({ "assertions": [{ "name": format!("{job} {state}"),
                "condition": { "events_contain": { "kind": tsi::EVENT_KIND, "contains": format!("{job} {state}") } } }] }),
        );
        assert_eq!(report["pass"], true, "{report}");
    }

    /// Starts an import and waits until it is staged; answers `(job, preview)`.
    fn stage(&mut self, params: Value) -> (String, Value) {
        let started = self.call("editor.terrain_source_start", params);
        let job = started["job"].as_str().expect("job").to_owned();
        self.wait_for(&job, "staged");
        self.expect_event(&job, "staged");
        let status = self.call("editor.terrain_source_status", json!({ "job": job }));
        assert_eq!(status["state"], "staged", "{status}");
        (job, status["preview"].clone())
    }

    fn heights(&mut self, e: Entity) -> Vec<u32> {
        self.world()
            .get::<Terrain>(e)
            .expect("terrain")
            .map
            .heights()
            .iter()
            .map(|h| h.to_bits())
            .collect()
    }

    fn undo_depth(&mut self) -> usize {
        self.world().resource::<UndoStack>().undo_depth()
    }
}

/// A 65x65 terrain, 64 m wide (1 m cells, first sample at (-32, -32)), heights -5..45 m.
fn spawn_terrain(world: &mut World) -> Entity {
    let heights = (0..SIDE * SIDE)
        .map(|i| 10.0 + ((i % SIDE) as f32 * 0.3).sin() * 6.0 + ((i / SIDE) as f32 * 0.17).cos() * 4.0)
        .collect();
    let map = TerrainMap::from_heights(SIDE, SIDE, heights, 64.0, 50.0, -5.0).expect("map");
    let terrain = Terrain::new(
        &map.to_heightfield(),
        64.0,
        50.0,
        -5.0,
        CdlodConfig::default(),
        2,
        TerrainLayers::default(),
    )
    .expect("terrain");
    world.spawn(terrain).id()
}

/// A schema-2 JSON count source of the terrain's grid: `count(col, row)`, registered on the
/// terrain's samples unless `registered` is false (then the reader's default first sample, half a
/// cell off).
fn source_json(count: impl Fn(u32, u32) -> u16, registered: bool) -> Vec<u8> {
    let counts: Vec<u16> = (0..SIDE * SIDE).map(|i| count(i % SIDE, i / SIDE)).collect();
    let mut grid = json!({ "cols": SIDE, "rows": SIDE, "cell_m": 1.0 });
    if registered {
        grid["sample0_x_m"] = json!(-32.0);
        grid["sample0_z_m"] = json!(-32.0);
    }
    json!({ "schema": 2, "grid": grid,
        "units": { "kind": "counts", "meters_per_count": MPC, "zero_counts": ZERO },
        "counts": counts })
    .to_string()
    .into_bytes()
}

fn gentle(col: u32, row: u32) -> u16 {
    ZERO + 1000 + ((col * 7 + row * 3) % 200) as u16
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    regia_agent::bake_export::write_atomic(path, bytes).expect("write");
}

/// Every file under `root` with its bytes.
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.insert(path.clone(), std::fs::read(&path).expect("read"));
            }
        }
    }
    out
}

fn code(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[test]
fn cli_agent_and_panel_previews_agree_on_bounds_and_datum() {
    let mut client = Client::new("tsw_agree");
    spawn_terrain(client.world());
    write_atomic(&client.root.join("assets/src.json"), &source_json(gentle, true));
    let params = json!({ "path": "assets/src.json" });

    // The CLI's service: what `regia terrain source-inspect` prints.
    let cli = regia_agent::read_project_source(&client.root, Default::default(), &params).expect("cli read");
    let cli = regia_agent::source_summary(&cli.source, cli.limits);
    // The agent's inspection.
    let agent = client.call("terrain.inspect_source", params.clone());
    // The editor workflow's preview (the panel shows it).
    let (_, preview) = client.stage(params);
    for (name, v) in [("cli", &cli), ("agent", &agent), ("editor", &preview["source"])] {
        assert_eq!(v["world_min"], json!([-32.0, -32.0]), "{name}: {v}");
        assert_eq!(v["world_max"], json!([32.0, 32.0]), "{name}: {v}");
        assert_eq!(v["zero_counts"], ZERO, "{name}");
        assert_eq!(v["meters_per_count"], json!(MPC), "{name}");
    }
    assert_eq!(cli, agent, "the CLI and agent previews are one value");
    assert_eq!(agent, preview["source"], "the editor's preview is the agent's");
    // The panel's text is formatted from that value.
    let lines = tsi::preview_lines(&preview);
    assert!(
        lines[0].contains(&format!("datum {ZERO} counts"))
            && lines[0].contains(&format!("{} m per count", json!(MPC))),
        "{lines:?}"
    );
    assert!(lines[1].contains("(-32.0, -32.0) to (32.0, 32.0)"), "{lines:?}");
    assert!(lines[2].starts_with("effective limits"), "{lines:?}");
    assert_eq!(lines[4], "mapping matches the terrain");
}

#[test]
fn a_wrong_destination_mapping_requires_an_explicit_conversion() {
    let mut client = Client::new("tsw_mapping");
    let e = spawn_terrain(client.world());
    // Half a cell off the terrain's samples, and two samples above the terrain's 45 m top.
    write_atomic(
        &client.root.join("assets/off.json"),
        &source_json(
            |c, r| {
                if c == 0 && r < 2 {
                    ZERO + 6000
                } else {
                    gentle(c, r)
                }
            },
            false,
        ),
    );
    let before = client.heights(e);
    let (job, preview) = client.stage(json!({ "path": "assets/off.json" }));
    assert_eq!(preview["mapping"]["registration_match"], false, "{preview}");
    assert_eq!(preview["mapping"]["registration_shift_m"], json!([-0.5, -0.5]));
    assert_eq!(
        preview["mapping"]["needs"],
        json!(["registration", "clamp_heights"])
    );
    assert_eq!(preview["changes"]["clamped_high"], 2);

    let refused = client
        .try_call("editor.terrain_source_apply", json!({ "job": job }))
        .expect_err("no conversion chosen");
    assert_eq!(code(&refused), "terrain_source_mapping_mismatch");
    assert_eq!(
        refused.data.as_ref().expect("data")["needs"],
        json!(["registration", "clamp_heights"])
    );
    let partly = client
        .try_call(
            "editor.terrain_source_apply",
            json!({ "job": job, "conversion": ["registration"] }),
        )
        .expect_err("one conversion missing");
    assert_eq!(code(&partly), "terrain_source_mapping_mismatch");
    assert_eq!(
        partly.data.as_ref().expect("data")["needs"],
        json!(["clamp_heights"])
    );
    assert_eq!(client.heights(e), before, "a refused apply changes no height");

    let applied = client.call(
        "editor.terrain_source_apply",
        json!({ "job": job, "conversion": ["registration", "clamp_heights"] }),
    );
    assert_eq!(applied["state"], "applied", "{applied}");
    let top = client.world().get::<Terrain>(e).expect("t").map.heights()[0];
    assert_eq!(top, 45.0, "clamped to the terrain's top");
    client.expect_event(&job, "applied");
}

#[test]
fn cancellation_before_commit_changes_no_asset() {
    let mut client = Client::new("tsw_cancel");
    let e = spawn_terrain(client.world());
    write_atomic(&client.root.join("assets/src.json"), &source_json(gentle, true));
    let files = tree(&client.root.join("assets"));
    let state = regia_core::brand::project_state_dir(&client.root);
    let heights = client.heights(e);
    let depth = client.undo_depth();

    // While the worker runs: it is held before the read by a gate, cancelled, then released.
    let (release, gate) = std::sync::mpsc::channel::<()>();
    client
        .world()
        .insert_resource(tsi::TerrainSourceStartGate(std::sync::Mutex::new(Some(gate))));
    let started = client.call(
        "editor.terrain_source_start",
        json!({ "path": "assets/src.json" }),
    );
    let job = started["job"].as_str().expect("job").to_owned();
    let status = client.call("editor.terrain_source_status", json!({ "job": job }));
    assert_eq!(status["state"], "running", "{status}");
    let cancelled = client.call("editor.terrain_source_cancel", json!({ "job": job }));
    assert_eq!(cancelled["changed_assets"], false);
    release.send(()).expect("release");
    client.wait_for(&job, "cancelled");
    let status = client.call("editor.terrain_source_status", json!({ "job": job }));
    assert_eq!(status["state"], "cancelled", "{status}");

    // Once staged: dropped at once.
    let (job2, _) = client.stage(json!({ "path": "assets/src.json" }));
    let dropped = client.call("editor.terrain_source_cancel", json!({ "job": job2 }));
    assert_eq!(dropped["state"], "cancelled", "{dropped}");
    client.expect_event(&job2, "cancelled");
    let late = client
        .try_call("editor.terrain_source_apply", json!({ "job": job2 }))
        .expect_err("nothing staged");
    assert_eq!(code(&late), "terrain_source_not_found");

    assert_eq!(
        tree(&client.root.join("assets")),
        files,
        "no asset was written, moved or removed"
    );
    assert!(!state.join(tsi::RECORDS_FILE).exists(), "no import was recorded");
    assert!(
        !state.join("terrain_source_backups").exists(),
        "no backup was written"
    );
    assert_eq!(client.heights(e), heights, "no height changed");
    assert_eq!(client.undo_depth(), depth, "no undo entry");
}

#[test]
fn a_source_edit_during_preview_refuses_a_stale_apply() {
    let mut client = Client::new("tsw_stale");
    let e = spawn_terrain(client.world());
    let path = client.root.join("assets/src.json");
    write_atomic(&path, &source_json(gentle, true));
    let heights = client.heights(e);
    let (job, _) = client.stage(json!({ "path": "assets/src.json" }));
    write_atomic(&path, &source_json(|c, r| gentle(c, r) + 1, true));
    let stale = client
        .try_call("editor.terrain_source_apply", json!({ "job": job }))
        .expect_err("stale");
    assert_eq!(code(&stale), "terrain_source_stale");
    assert_eq!(client.heights(e), heights);
    let status = client.call("editor.terrain_source_status", json!({ "job": job }));
    assert_eq!(status["state"], "failed", "a stale preview ends: {status}");
    assert_eq!(status["error"]["code"], "terrain_source_stale");

    // A terrain edited during the preview refuses too.
    let (job, _) = client.stage(json!({ "path": "assets/src.json" }));
    {
        let world = client.world();
        let mut hf = world.get::<Terrain>(e).expect("t").map.to_heightfield();
        hf.samples[0] = hf.samples[0].wrapping_add(1);
        let mut edits = regia_terrain::TerrainEdits::default();
        let mut terrain = world.get_mut::<Terrain>(e).expect("t");
        regia_terrain::import_heights(&mut terrain, &mut edits, &hf).expect("edit");
    }
    let changed = client
        .try_call("editor.terrain_source_apply", json!({ "job": job }))
        .expect_err("destination changed");
    assert_eq!(code(&changed), "terrain_source_destination_changed");
}

#[test]
fn a_small_import_is_one_undo_step_that_restores_bits() {
    let mut client = Client::new("tsw_undo");
    let e = spawn_terrain(client.world());
    write_atomic(&client.root.join("assets/src.json"), &source_json(gentle, true));
    let before = client.heights(e);
    let depth = client.undo_depth();
    let (job, preview) = client.stage(json!({ "path": "assets/src.json" }));
    assert_eq!(preview["undo"]["undoable"], true);
    let changed = preview["changes"]["changed"].as_u64().expect("changed");
    assert!(changed > 0);
    let applied = client.call("editor.terrain_source_apply", json!({ "job": job }));
    assert_eq!(applied["changed"], changed, "the preview's count is the apply's");
    assert_eq!(applied["undoable"], true);
    assert_eq!(applied["backup"], Value::Null);
    assert_eq!(client.undo_depth(), depth + 1, "one undo step");
    let after = client.heights(e);
    assert_ne!(after, before);
    assert!(
        client
            .world()
            .resource_scope::<UndoStack, bool>(|w, mut s| s.undo(w))
    );
    assert_eq!(client.heights(e), before, "undo restores every height bit");
    assert!(
        client
            .world()
            .resource_scope::<UndoStack, bool>(|w, mut s| s.redo(w))
    );
    assert_eq!(client.heights(e), after);
}

#[test]
fn a_large_import_cannot_silently_discard_recoverability() {
    let mut client = Client::new("tsw_large");
    let e = spawn_terrain(client.world());
    client.world().insert_resource(tsi::TerrainSourceImportPolicy {
        undo_limit_samples: 100,
    });
    write_atomic(&client.root.join("assets/src.json"), &source_json(gentle, true));
    let original = client.world().get::<Terrain>(e).expect("t").map.to_heightfield();
    let before = client.heights(e);
    let (job, preview) = client.stage(json!({ "path": "assets/src.json" }));
    assert_eq!(preview["undo"]["limit"], 100);
    assert_eq!(preview["undo"]["undoable"], false, "disclosed before apply");
    assert_eq!(preview["undo"]["acknowledge_required"], true);
    let lines = tsi::preview_lines(&preview);
    assert!(lines[6].contains("over the undo limit of 100"), "{lines:?}");

    let refused = client
        .try_call("editor.terrain_source_apply", json!({ "job": job }))
        .expect_err("needs acknowledgement");
    assert_eq!(code(&refused), "terrain_source_undo_limit");
    assert_eq!(refused.data.as_ref().expect("data")["undo_limit"], 100);
    assert_eq!(client.heights(e), before);

    let applied = client.call(
        "editor.terrain_source_apply",
        json!({ "job": job, "acknowledge_undo_limit": true }),
    );
    assert_eq!(applied["undoable"], false, "{applied}");
    let backup = client.root.join(applied["backup"].as_str().expect("backup path"));
    let saved =
        regia_editor::terrain_panel::decode_png16(&std::fs::read(&backup).expect("backup")).expect("png");
    assert_eq!(
        saved.samples, original.samples,
        "the backup holds the original heights"
    );
    // The Terrain panel's heightmap import of the backup restores the original bits.
    regia_editor::terrain_panel::import_heightmap(
        client.world(),
        e,
        &backup,
        regia_editor::terrain_panel::HeightmapFormat::Png,
        None,
    )
    .expect("restore");
    assert_eq!(client.heights(e), before, "recovered bit for bit");
}

#[test]
fn reimport_uses_the_record_and_recooks_only_dependent_tiles() {
    let mut client = Client::new("tsw_reimport");
    let e = spawn_terrain(client.world());
    let cache = client.root.join("cache");
    let map_id = regia_asset::AssetId::new_random();
    client
        .world()
        .entity_mut(e)
        .insert(regia_editor::terrain_tool::TerrainCookTarget {
            map_id,
            cache_dir: cache.clone(),
        });
    std::fs::create_dir_all(&cache).expect("cache");
    // The terrain's cooked heightfield in 32-sample tiles (a 2x2 grid of tiles for 65 samples), so a
    // local change can re-cook part of it; the save's default tile size would cover it with one tile.
    {
        use regia_asset_pipeline::terrain_data::{
            HeightfieldTile, TiledHeightfield, write_tiled_heightfield_cooked,
        };
        let map = &client.world().get::<Terrain>(e).expect("t").map;
        let cooked = regia_terrain::cook_all_tiles(map, 32).expect("cook");
        let tiled = TiledHeightfield {
            version: cooked.version,
            width: cooked.width,
            height: cooked.height,
            tile_size: cooked.tile_size,
            tiles: cooked
                .tiles
                .into_iter()
                .map(|t| HeightfieldTile {
                    x: t.x,
                    y: t.y,
                    min: t.min,
                    max: t.max,
                    samples: t.samples,
                })
                .collect(),
        };
        write_tiled_heightfield_cooked(map_id, &tiled, &cache).expect("cooked");
    }
    write_atomic(&cache.join("unrelated.fckd"), b"another asset's cooked bytes");
    let path = client.root.join("assets/src.json");
    // The first import is half a cell off: its registration conversion is recorded too.
    write_atomic(&path, &source_json(gentle, false));
    let (job, _) = client.stage(json!({ "path": "assets/src.json" }));
    client.call(
        "editor.terrain_source_apply",
        json!({ "job": job, "conversion": ["registration"] }),
    );
    let first = regia_editor::terrain_tool::save_terrain_edits(client.world()).expect("save");
    assert_eq!(first.len(), 1);
    let cooked_path = first[0].path.clone();
    let cooked_before = std::fs::read(&cooked_path).expect("cooked");

    // One sample of the source changes; reimport repeats the recorded source and conversion.
    write_atomic(
        &path,
        &source_json(
            |c, r| {
                if (c, r) == (10, 10) {
                    gentle(c, r) + 300
                } else {
                    gentle(c, r)
                }
            },
            false,
        ),
    );
    let started = client.call("editor.terrain_source_reimport", json!({ "entity": e.to_bits() }));
    assert_eq!(started["source"]["path"], "assets/src.json");
    assert_eq!(started["conversion"], json!(["registration"]));
    let job = started["job"].as_str().expect("job").to_owned();
    client.wait_for(&job, "staged");
    let status = client.call("editor.terrain_source_status", json!({ "job": job }));
    let changes = &status["preview"]["changes"];
    assert_eq!(changes["changed"], 1, "{status}");
    assert_eq!(
        changes["dirty"],
        json!({ "col_start": 10, "col_end": 11, "row_start": 10, "row_end": 11 })
    );
    // The recorded conversion applies without being chosen again.
    let applied = client.call("editor.terrain_source_apply", json!({ "job": job }));
    assert_eq!(applied["changed"], 1);
    let unrelated = std::fs::read(cache.join("unrelated.fckd")).expect("unrelated");
    let second = regia_editor::terrain_tool::save_terrain_edits(client.world()).expect("save");
    assert_eq!(second.len(), 1);
    assert!(
        second[0].tiles_recooked >= 1 && second[0].tiles_recooked < second[0].tiles_total,
        "only the changed tiles re-cook: {:?}",
        second[0]
    );
    assert_eq!(
        std::fs::read(cache.join("unrelated.fckd")).expect("unrelated"),
        unrelated,
        "other cooked outputs are untouched"
    );
    let a = regia_terrain::decode_cooked_tiled_heightfield(&cooked_before).expect("decode");
    let b = regia_terrain::decode_cooked_tiled_heightfield(&std::fs::read(&cooked_path).expect("cooked"))
        .expect("decode");
    let differing = a
        .tiles
        .iter()
        .zip(&b.tiles)
        .filter(|(x, y)| x.samples != y.samples)
        .count();
    assert!(
        differing >= 1 && differing <= second[0].tiles_recooked,
        "{differing} tiles differ"
    );

    // A terrain without a record refuses a reimport.
    let other = spawn_terrain(client.world());
    let none = client
        .try_call(
            "editor.terrain_source_reimport",
            json!({ "entity": other.to_bits() }),
        )
        .expect_err("no record");
    assert_eq!(code(&none), "terrain_source_no_record");
}

/// `regia terrain source-import`'s service: a dry run writes nothing; the import publishes the
/// counts verbatim and the source's schema-2 mapping; a destination that maps counts differently
/// needs the explicit `adopt_source` choice; another grid is refused (never resampled).
#[test]
fn cli_source_import_publishes_exactly_and_needs_a_choice_for_another_mapping() {
    let root = regia_test_utils::unique_temp_dir("tsw_cli_import");
    std::fs::create_dir_all(root.join("assets/maps/hills")).expect("dirs");
    write_atomic(&root.join("assets/src.json"), &source_json(gentle, true));
    let params = json!({ "path": "assets/src.json" });
    let dest = "assets/maps/hills/heightfield.png";
    let policy = regia_terrain::source::ImportLimits::default();

    let dry = regia_agent::publish_source_asset(&root, policy, &params, dest, false, true).expect("dry run");
    assert_eq!(dry["dry_run"], true);
    assert!(!root.join(dest).exists(), "a dry run writes nothing");

    let done = regia_agent::publish_source_asset(&root, policy, &params, dest, false, false).expect("import");
    assert_eq!(
        done["written"],
        json!([dest, "assets/maps/hills/heightfield.json"])
    );
    let read = regia_agent::read_project_source(&root, policy, &params).expect("read");
    let png = regia_editor::terrain_panel::decode_png16(&std::fs::read(root.join(dest)).expect("png"))
        .expect("16-bit");
    assert_eq!(png.samples, read.source.counts(), "counts verbatim");
    let mapping = regia_terrain::source::CountMapping::from_heightfield_json(
        &std::fs::read(root.join("assets/maps/hills/heightfield.json")).expect("json"),
    )
    .expect("parse")
    .expect("schema 2");
    assert_eq!(
        mapping.canonical_bytes(),
        read.source.mapping().canonical_bytes(),
        "the cook reads the source's datum, scale and registration back exactly"
    );

    // Another registration: refused until adopted, and nothing is rewritten meanwhile.
    write_atomic(&root.join("assets/off.json"), &source_json(gentle, false));
    let off = json!({ "path": "assets/off.json" });
    let before = tree(&root);
    let refused =
        regia_agent::publish_source_asset(&root, policy, &off, dest, false, false).expect_err("mismatch");
    assert_eq!(code(&refused), "terrain_source_mapping_mismatch");
    assert_eq!(tree(&root), before);
    regia_agent::publish_source_asset(&root, policy, &off, dest, true, false).expect("adopted");

    // Another grid size is never resampled.
    let small = json!({ "schema": 2, "grid": { "cols": 3, "rows": 2, "cell_m": 1.0 },
        "units": { "kind": "counts", "meters_per_count": MPC, "zero_counts": ZERO },
        "counts": [1, 2, 3, 4, 5, 6] });
    write_atomic(&root.join("assets/small.json"), small.to_string().as_bytes());
    let grid = regia_agent::publish_source_asset(
        &root,
        policy,
        &json!({ "path": "assets/small.json" }),
        dest,
        true,
        false,
    )
    .expect_err("grid");
    assert_eq!(code(&grid), "terrain_source_grid_mismatch");
}
