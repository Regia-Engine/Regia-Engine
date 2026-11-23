// SPDX-License-Identifier: MIT-0
//! Lifecycle regression checks through the existing agent protocol, with read-only ECS probes.
//! On Linux run with Vulkan restricted to lavapipe; an absent device is a test failure on every host.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use procgen_common::{call, drive, generator};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::exec::{GeneratedMesh, InstanceSet, TerrainEditRecord};
use regia_procgen::world::{ProcGraphOverlay, ProcInstanceCollider, ProcOutput, ProcOutputRoot};
use regia_scene::components::{EntityGuid, Generated, Name};
use regia_scene::{GlobalTransform, Transform};
use regia_terrain::Terrain;
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

const AUTHORED: &str = "authored lifecycle sentinel";
const AUTHORED_GUID: Guid = Guid(0xc1ea_0064);

type Rpc<'a> = dyn FnMut(&str, Value) -> Value + 'a;

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let path = unique_temp_dir("roads-clear-regenerate");
        std::fs::create_dir_all(&path).expect("create isolated test project");
        Self(path)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("cannot clean test project {}: {error}", self.0.display());
            assert!(std::thread::panicking(), "test project cleanup failed");
        }
    }
}

/// Explicit fields avoid comparing entity IDs, run counters, cache timings or temporary paths.
#[derive(Clone, Debug, PartialEq)]
struct Content {
    sets: Vec<InstanceSet>,
    meshes: Vec<GeneratedMesh>,
    edits: Vec<TerrainEditRecord>,
}

#[derive(Clone)]
struct Snapshot {
    heights: Vec<u32>,
    content: Option<Content>,
    roots: usize,
    generated: usize,
    colliders: Vec<(String, usize)>,
    authored: Option<(String, Transform)>,
}

fn snapshot(world: &mut World) -> Snapshot {
    let mut terrain = world.query::<&Terrain>();
    let terrains: Vec<_> = terrain.iter(world).collect();
    assert_eq!(terrains.len(), 1);
    let heights = terrains[0].map.heights().iter().map(|h| h.to_bits()).collect();
    let outputs: Vec<_> = world.query::<&ProcOutput>().iter(world).collect();
    assert!(outputs.len() <= 1, "one generator publishes at most one output");
    let content = outputs.first().map(|o| {
        // These outputs are unused by this fixture; fail if its content surface changes silently.
        assert!(o.0.prefabs.is_empty() && o.0.tables.is_empty() && o.0.points.is_empty());
        assert!(o.0.masks.is_empty() && o.0.graph_outputs.is_empty());
        assert!(o.0.road_decals.is_empty() && o.0.exports.is_empty());
        Content {
            sets: o.0.instance_sets.clone(),
            meshes: o.0.meshes.clone(),
            edits: o.0.terrain_edits.clone(),
        }
    });
    let roots = world.query::<&ProcOutputRoot>().iter(world).count();
    let generated = world.query::<&Generated>().iter(world).count();
    let mut colliders: Vec<_> = world
        .query::<&ProcInstanceCollider>()
        .iter(world)
        .map(|c| (format!("{:?}", c.kind).to_lowercase(), c.instances))
        .collect();
    colliders.sort();
    let authored = world
        .query::<(&EntityGuid, &Name, &Transform)>()
        .iter(world)
        .find(|(guid, _, _)| guid.0 == AUTHORED_GUID)
        .map(|(_, name, transform)| (name.as_str().to_owned(), *transform));
    Snapshot {
        heights,
        content,
        roots,
        generated,
        colliders,
        authored,
    }
}

/// On Linux the Vulkan loader must be restricted to lavapipe (a card there is never chosen, and an
/// absent software device fails the test, never skips it). Other hosts have no Vulkan driver list to
/// restrict: the checks here read world state and reports, not pixels, so they run on the platform's
/// adapter (Metal on the Apple M5, ADR-7335). An absent device still fails in `procgen_roads::app`.
fn require_lavapipe_on_linux() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let files = std::env::var_os("VK_DRIVER_FILES").or_else(|| std::env::var_os("VK_ICD_FILENAMES"));
    let Some(drivers) = files else {
        // The loader's driver filter restricts it as well (the merge gate's lavapipe pass sets
        // only `VK_LOADER_DRIVERS_SELECT=*lvp*`, ADR-7419): every pattern must name lavapipe.
        let select = std::env::var("VK_LOADER_DRIVERS_SELECT").unwrap_or_default();
        assert!(
            !select.is_empty() && select.split(',').all(|pattern| pattern.contains("lvp")),
            "restrict Vulkan to lavapipe with VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json \
             or VK_LOADER_DRIVERS_SELECT=*lvp* (got {select:?})"
        );
        return;
    };
    let drivers: Vec<_> = std::env::split_paths(&drivers).collect();
    assert_eq!(drivers.len(), 1, "lavapipe alone is required");
    assert!(
        drivers[0]
            .file_name()
            .is_some_and(|f| f.to_string_lossy().starts_with("lvp_icd"))
    );
}

// The probe only reads the world. All lifecycle mutations use real protocol calls in drive().
struct Probe(mpsc::Sender<mpsc::SyncSender<Snapshot>>);

impl Probe {
    fn read(&self, rpc: &mut Rpc<'_>) -> Snapshot {
        rpc("play.step", json!({ "frames": 2 }));
        let (send, receive) = mpsc::sync_channel(1);
        self.0.send(send).expect("request read-only snapshot");
        receive
            .recv_timeout(Duration::from_secs(60))
            .expect("snapshot hang guard")
    }
}

fn fixture(project: &Path, bake: Option<PathBuf>, collision: bool) -> (App, Probe, Snapshot) {
    require_lavapipe_on_linux();
    let mut app = procgen_roads::app(project, bake)
        .expect("a render device is required (lavapipe on Linux); never skip this test");
    let world = app.world_mut();
    if collision {
        // Opt into existing collision modes without modifying the shipped graph or dependencies.
        let graph = include_str!("../assets/road_network.pgraph.ron");
        let curb = "mesh: \"props/curb_stone\", weight: 1.0";
        let rock = "mesh: \"props/rock\", weight: 1.0";
        assert_eq!(graph.matches(curb).count(), 1);
        assert_eq!(graph.matches(rock).count(), 1);
        let graph = graph
            .replace(curb, &format!("{curb}, collision: \"Simple\""))
            .replace(rock, &format!("{rock}, collision: \"Mesh\""));
        world.insert_resource(ProcGraphOverlay(
            [("road_network.pgraph.ron".into(), graph)].into(),
        ));
    }
    world.spawn((
        EntityGuid(AUTHORED_GUID),
        Name::new(AUTHORED),
        Transform::from_translation(Vec3::new(250.0, 30.0, 250.0)),
        GlobalTransform::default(),
    ));
    let original = snapshot(world);
    assert_eq!(original.heights.len(), 257 * 257);
    assert_eq!(original.authored.as_ref().map(|a| a.0.as_str()), Some(AUTHORED));
    let (send, receive) = mpsc::channel::<mpsc::SyncSender<Snapshot>>();
    let receive = Mutex::new(receive);
    app.add_systems(Last, move |world: &mut World| {
        if let Ok(reply) = receive.lock().expect("probe lock").try_recv() {
            let _ = reply.send(snapshot(world));
        }
    });
    (app, Probe(send), original)
}

fn assert_same_heights(actual: &[u32], expected: &[u32], stage: &str) {
    assert_eq!(actual.len(), expected.len(), "{stage}: heightfield dimensions");
    let changed = actual.iter().zip(expected).filter(|(a, b)| a != b).count();
    assert_eq!(changed, 0, "{stage}: terrain samples differ bit for bit");
}

fn assert_content(actual: &Snapshot, expected: &Snapshot) {
    assert!(
        actual.content == expected.content,
        "normalized mesh/instance/edit content changed"
    );
    assert_same_heights(&actual.heights, &expected.heights, "regenerated flattened ground");
    assert_eq!(actual.roots, expected.roots, "output roots accumulate");
    assert_eq!(
        actual.generated, expected.generated,
        "generated entities accumulate"
    );
    assert_eq!(
        actual.colliders, expected.colliders,
        "collision output accumulates"
    );
    assert_eq!(actual.authored, expected.authored, "authored content changed");
}

fn roads(rpc: &mut Rpc<'_>, entity: u64) -> Value {
    let report = rpc("procgen.report", json!({ "entity": entity }));
    assert_eq!(report["totals"]["junctions"], 2, "{report}");
    let roads = report["roads"].as_array().expect("per-road report");
    assert_eq!(roads.len(), 3);
    assert_eq!(
        roads
            .iter()
            .map(|r| r["junctions"].as_u64().expect("junction count"))
            .sum::<u64>(),
        4
    );
    let mut normalized: Vec<_> = roads
        .iter()
        .map(|r| {
            json!({
                "length_m": r["length_m"], "junctions": r["junctions"],
                "flatten_rect": r["flatten_rect"], "flatten_limit_at_m": r["flatten_limit_at_m"],
            })
        })
        .collect();
    normalized.sort_by_key(Value::to_string);
    json!(normalized)
}

fn draw(rpc: &mut Rpc<'_>) -> Value {
    let list = rpc("procgen.list", json!({}));
    assert_eq!(list["generators"].as_array().expect("generators").len(), 1);
    let draw = list["draw"].clone();
    assert_eq!(draw["missing_meshes"], json!([]), "{draw}");
    assert_eq!(draw["missing_materials"], json!([]), "{draw}");
    draw
}

fn run(rpc: &mut Rpc<'_>, entity: u64) {
    let result = rpc("procgen.run", json!({ "entity": entity, "wait": true }));
    assert_eq!(result["state"], "ready", "{result}");
}

fn clear(rpc: &mut Rpc<'_>, entity: u64, probe: &Probe, original: &Snapshot) {
    let result = rpc("procgen.clear", json!({ "entity": entity }));
    assert_eq!(result["cleared"], true, "{result}");
    let cleared = probe.read(rpc);
    assert_same_heights(
        &cleared.heights,
        &original.heights,
        "clear restores original synthetic ground",
    );
    assert!(cleared.content.is_none(), "clear removes the published output");
    assert_eq!(cleared.roots, 0);
    assert_eq!(cleared.generated, 0);
    assert!(cleared.colliders.is_empty());
    assert_eq!(
        cleared.authored, original.authored,
        "clear removed unrelated authored content"
    );
    let list = rpc("procgen.list", json!({}));
    assert_eq!(list["generators"][0]["state"], "cleared", "{list}");
    assert_eq!(list["generators"][0]["output"]["instances"], 0);
    let draw = draw(rpc);
    for key in ["drawn", "culled", "generated_meshes", "generated_triangles"] {
        assert_eq!(draw[key], 0, "clear left draw output: {draw}");
    }
}

#[test]
fn run_clear_run_restores_and_reproduces_three_cycles() {
    let project = Project::new();
    let (app, probe, original) = fixture(&project.0, None, false);
    drive(app, &project.0, move |client| {
        let entity = generator(client, procgen_roads::GENERATOR);
        let rpc = &mut |method: &str, params| call(client, method, params);
        run(rpc, entity);
        let first = probe.read(rpc);
        assert_ne!(
            first.heights, original.heights,
            "flattening must change real samples"
        );
        assert_eq!(first.roots, 1);
        assert_eq!(first.authored, original.authored);
        assert_eq!(first.generated, 1, "only the generated output root");
        assert!(first.colliders.is_empty(), "shipped graph requests no collision");
        assert!(
            first
                .content
                .as_ref()
                .is_some_and(|c| !c.edits.is_empty() && c.meshes.len() >= 3)
        );
        let first_roads = roads(rpc, entity);
        let first_draw = draw(rpc);
        assert!(first_draw["drawn"].as_u64() > Some(500), "{first_draw}");
        assert!(first_draw["generated_triangles"].as_u64() > Some(0));
        for _ in 0..3 {
            clear(rpc, entity, &probe, &original);
            run(rpc, entity);
            assert_content(&probe.read(rpc), &first);
            assert_eq!(roads(rpc, entity), first_roads);
            assert_eq!(draw(rpc), first_draw);
        }
        clear(rpc, entity, &probe, &original);
    });
}

#[test]
fn bake_clear_run_and_separate_fresh_app_reload_reproduce_content() {
    let project = Project::new();
    let bake_dir = project.0.join("level.procgen");
    let (app, probe, original) = fixture(&project.0, Some(bake_dir.clone()), true);
    let first = drive(app, &project.0, move |client| {
        let entity = generator(client, procgen_roads::GENERATOR);
        let rpc = &mut |method: &str, params| call(client, method, params);
        run(rpc, entity);
        let first = probe.read(rpc);
        assert_ne!(first.heights, original.heights);
        assert_eq!(first.roots, 1);
        assert_eq!(first.authored, original.authored);
        assert_eq!(first.generated, 3, "one root and two generated colliders");
        assert_eq!(
            first.colliders.len(),
            2,
            "exercise nonempty simple and mesh collision"
        );
        assert_eq!(first.colliders[0].0, "mesh");
        assert_eq!(first.colliders[1].0, "simple");
        assert!(first.colliders.iter().all(|(_, count)| *count > 0));
        let first_roads = roads(rpc, entity);
        let first_draw = draw(rpc);
        for _ in 0..3 {
            let bake = rpc("procgen.bake", json!({ "entity": entity }));
            assert_eq!(bake["ok"], true, "{bake}");
            assert_content(&probe.read(rpc), &first);
            let verify = rpc("procgen.verify", json!({ "entity": entity }));
            assert_eq!(verify["ok"], true, "{verify}");
            assert_content(&probe.read(rpc), &first);
            clear(rpc, entity, &probe, &original);
            run(rpc, entity);
            assert_content(&probe.read(rpc), &first);
            assert_eq!(roads(rpc, entity), first_roads);
            assert_eq!(draw(rpc), first_draw);
        }
        clear(rpc, entity, &probe, &original);
        (first, first_draw, first_roads)
    });
    assert_eq!(std::fs::read_dir(&bake_dir).expect("bake directory").count(), 1);

    // New project/server directory and new App: loading is distinct from an in-session regenerate.
    let reload = Project::new();
    let (app, probe, original) = fixture(&reload.0, Some(bake_dir), true);
    drive(app, &reload.0, move |client| {
        let entity = generator(client, procgen_roads::GENERATOR);
        let rpc = &mut |method: &str, params| call(client, method, params);
        let loaded = probe.read(rpc);
        let list = rpc("procgen.list", json!({}));
        assert_eq!(list["generators"][0]["state"], "ready", "{list}");
        assert_eq!(
            list["generators"][0]["run_id"], 0,
            "reload must not run the graph"
        );
        assert_content(&loaded, &first.0);
        assert_eq!(draw(rpc), first.1);
        let verify = rpc("procgen.verify", json!({ "entity": entity }));
        assert_eq!(verify["ok"], true, "{verify}");
        assert_content(&probe.read(rpc), &first.0);
        clear(rpc, entity, &probe, &original);
        run(rpc, entity);
        assert_content(&probe.read(rpc), &first.0);
        assert_eq!(roads(rpc, entity), first.2);
        assert_eq!(draw(rpc), first.1);
        clear(rpc, entity, &probe, &original);
    });
}
