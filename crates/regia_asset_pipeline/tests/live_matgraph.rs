// SPDX-License-Identifier: MPL-2.0
//! Material-graph live reload end to end (`docs/specs/12-assets.md` § 8): a running app with the
//! [`AssetServer`], the [`MaterialGraphPlugin`] runtime (ADR-081) and a [`LiveCooker`]. Saving a
//! `.matgraph.ron` re-cooks the graph and its instances in the background; the cooker's
//! `Reload(id)` bumps the server's generation, [`GraphMaterials`] rebuilds the material and swaps
//! it on the entity within one second, and the new pipelines build on the renderer's workers
//! (never on the frame path). A graph that no longer compiles keeps the previous material and
//! reports the cook error.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetReloadInbox, AssetServerSettings, AssetWatchStatus, WatchOutcome};
use regia_asset_pipeline::{CookOptions, LiveCooker, LiveCookerConfig, cook_project, host_platform};
use regia_matgraph::{MaterialGraphPlugin, MaterialGraphRef};
use regia_render::{CpuMaterial, CpuMesh, MeshRenderer, RenderDevice, RuntimeRenderer};

/// The spec's reload budget, asserted in release builds only (see `assert_reload_budget`).
const DEADLINE: Duration = Duration::from_secs(1);

fn graph(op: &str) -> String {
    format!(
        r#"(
    format_version: 2,
    name: "tinted",
    params: [(name: "tint", value: Color((0.8, 0.4, 0.2)))],
    nodes: {{
        "tint": Param("tint"),
        "vcol": VertexColor,
        "rgb": Swizzle(input: "vcol", channels: "rgb"),
        "base": {op}("tint", "rgb"),
    }},
    output: (base_color: Some("base")),
)"#
    )
}

const INSTANCE: &str = r#"(
    format_version: 2,
    parent: "assets/materials/tinted.matgraph.ron",
    params: { "tint": Color((0.9, 0.1, 0.1)) },
)"#;

const GRAPH_PATH: &str = "assets/materials/tinted.matgraph.ron";
const INSTANCE_PATH: &str = "assets/materials/tinted_red.matinst.ron";

struct Harness {
    root: PathBuf,
    app: App,
    graph_entity: Entity,
    instance_entity: Entity,
    _cooker: LiveCooker,
}

impl Harness {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("regia_live_matgraph_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("assets/materials")).expect("dirs");
        std::fs::write(root.join(GRAPH_PATH), graph("Mul")).expect("graph");
        std::fs::write(root.join(INSTANCE_PATH), INSTANCE).expect("instance");
        let report = cook_project(CookOptions::for_host(root.clone())).expect("initial cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let (tx, inbox) = AssetReloadInbox::channel();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(inbox);
        app.add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: root.clone(),
                platform: host_platform().to_string(),
                content_dir: None,
            },
        })
        .add_plugins(regia_scene::ScenePlugin)
        .add_plugins(MaterialGraphPlugin);
        // A renderer on the device (lavapipe on the farm), so loaded graphs prewarm on workers.
        let device = RenderDevice::headless().unwrap_or_else(|e| panic!("no GPU: {e}"));
        let mut renderer = RuntimeRenderer::default();
        renderer.0.set_async_material_graphs(true);
        app.insert_resource(device).insert_resource(renderer);
        let mesh = Arc::new(CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let mut spawn = |path: &str| {
            app.world_mut()
                .spawn((
                    MeshRenderer::new(Arc::clone(&mesh), Arc::new(CpuMaterial::default())),
                    MaterialGraphRef::new(path),
                ))
                .id()
        };
        let graph_entity = spawn(GRAPH_PATH);
        let instance_entity = spawn(INSTANCE_PATH);
        let cooker = LiveCooker::spawn(LiveCookerConfig::for_host(root.clone()), tx).expect("live cooker");
        app.update();
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .settle_material_graphs();
        Self {
            root,
            app,
            graph_entity,
            instance_entity,
            _cooker: cooker,
        }
    }

    fn graph_key(&self, entity: Entity) -> u64 {
        self.app
            .world()
            .get::<MeshRenderer>(entity)
            .and_then(|r| r.material.surface_graph.as_ref().map(|g| g.key))
            .expect("a graph material")
    }

    /// Writes `text` to `rel`, then updates the app until `done` holds; returns the latency.
    fn edit_until(&mut self, rel: &str, text: &str, mut done: impl FnMut(&Self) -> bool) -> Duration {
        let path = self.root.join(rel);
        let start = Instant::now();
        write_atomically(&path, text.as_bytes());
        while start.elapsed() < DEADLINE * 10 {
            self.app.update();
            if done(self) {
                return start.elapsed();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("{rel}: no reload within {:?}", DEADLINE * 10);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Write-to-temp + rename, the way most editors save.
fn write_atomically(path: &Path, bytes: &[u8]) {
    let tmp = path.with_file_name(format!(
        ".regia-tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    std::fs::write(&tmp, bytes).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

#[test]
fn graph_edit_recooks_and_swaps_graph_and_instance_within_one_second() {
    let mut h = Harness::new();
    let (graph_before, instance_before) = (h.graph_key(h.graph_entity), h.graph_key(h.instance_entity));
    let stats_before = h
        .app
        .world()
        .resource::<RuntimeRenderer>()
        .0
        .material_graph_stats();
    assert!(
        stats_before.ready > 0,
        "the initial graphs prewarmed: {stats_before:?}"
    );

    let took = h.edit_until(GRAPH_PATH, &graph("Add"), |h| {
        h.graph_key(h.graph_entity) != graph_before && h.graph_key(h.instance_entity) != instance_before
    });
    assert_reload_budget(took, "graph + instance swap");

    // The new permutations build on the renderer's workers, never on the frame path.
    let settle = Instant::now();
    h.app
        .world_mut()
        .resource_mut::<RuntimeRenderer>()
        .0
        .settle_material_graphs();
    let stats = h
        .app
        .world()
        .resource::<RuntimeRenderer>()
        .0
        .material_graph_stats();
    eprintln!(
        "swap {took:?}, pipelines ready {:?} later: {stats:?}",
        settle.elapsed()
    );
    assert!(stats.ready > stats_before.ready, "{stats:?}");
    assert_eq!(stats.built_on_frame_path, 0, "{stats:?}");
    assert_eq!(stats.failed, 0, "{stats:?}");

    let status = h.app.world().resource::<AssetWatchStatus>();
    assert!(
        status
            .recent
            .iter()
            .any(|e| e.path.ends_with("tinted.matgraph.ron") && e.outcome == WatchOutcome::Reloaded),
        "{:?}",
        status.recent
    );
}

#[test]
fn broken_graph_keeps_the_previous_material_and_reports_the_cook_error() {
    let mut h = Harness::new();
    let before = h.graph_key(h.graph_entity);
    let took = h.edit_until(GRAPH_PATH, &graph("NoSuchNode"), |h| {
        h.app
            .world()
            .resource::<AssetWatchStatus>()
            .recent
            .iter()
            .any(|e| e.path.ends_with("tinted.matgraph.ron") && matches!(e.outcome, WatchOutcome::Error(_)))
    });
    assert_reload_budget(took, "cook error");
    for _ in 0..5 {
        h.app.update();
    }
    assert_eq!(h.graph_key(h.graph_entity), before, "the old material stays");

    // Fixing the file swaps again.
    let took = h.edit_until(GRAPH_PATH, &graph("Add"), |h| {
        h.graph_key(h.graph_entity) != before
    });
    assert_reload_budget(took, "fix swap");
}

/// The spec's one-second reload budget, asserted in release builds only: a debug test binary on a
/// loaded machine measures the scheduler, not the reload (audit-v035-flaky H1). Debug runs print it.
fn assert_reload_budget(took: Duration, what: &str) {
    eprintln!("{what} took {took:?}");
    if !cfg!(debug_assertions) {
        assert!(took < DEADLINE, "{what} took {took:?}");
    }
}
