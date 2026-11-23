// SPDX-License-Identifier: MPL-2.0
//! Perf-audit counter tests for anchored procgen tiles (lane sprint-fix-procgen2-perf-g).
//!
//! The counters these tests read (`graph_text_reads`, `run_output_clones`, `cells_in_rect_calls`) are
//! process-wide. Run beside the other `tiles` tests they also count those tests' work, so an exact
//! "no new work" assertion failed under parallel test threads. This binary holds only the counter
//! tests, and each one takes [`serial`] first, so no other counted work runs in the process while
//! it measures (moved out of `tests/tiles.rs` by integ-v047b; assertions unchanged).

mod common;

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use common::{graph, node};
use regia_core::math::Vec3;
use regia_procgen::ProcGraph;

/// One counter test at a time in this process.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `in.self → sample.region (jittered) → points.jitter (scale) → [prune] → spawn.mesh`.
fn scatter_graph(spacing: f64, jitter: f64, prune: bool) -> ProcGraph {
    let mut nodes = vec![
        (1, node("in.self", &[])),
        (
            2,
            node(
                "sample.region",
                &[
                    ("spacing_m", &format!("{spacing:?}")),
                    ("jitter", &format!("{jitter:?}")),
                ],
            ),
        ),
        (
            3,
            node(
                "points.jitter",
                &[
                    ("scale", "[(1.0, 1.0, 1.0), (4.0, 1.0, 4.0)]"),
                    ("scale_axes", "each"),
                ],
            ),
        ),
        (5, node("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
    ];
    let mut edges = vec![(1, "out", 2, "region"), (2, "out", 3, "in")];
    if prune {
        nodes.push((4, node("points.prune", &[("max_footprint_m", "4.0")])));
        edges.push((3, "out", 4, "in"));
        edges.push((4, "out", 5, "in"));
    } else {
        edges.push((3, "out", 5, "in"));
    }
    graph(nodes, &edges)
}

fn perf_memory_files() -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    files.insert(
        "s.pgraph.ron".to_string(),
        scatter_graph(8.0, 0.4, false).to_ron(),
    );
    files
}

fn perf_anchored(
    source: regia_procgen::world::ProcGraphSource,
    file_check_frames: u32,
) -> (regia_app::prelude::App, regia_app::prelude::Entity) {
    use regia_app::prelude::*;
    use regia_procgen::tiles::{AnchorSettings, AnchorSource, ProcGenAnchor, TileSettings};
    use regia_procgen::world::{ProcBounds, ProcGenMode, ProcGenPlugin, ProcGenSettings, ProcGenerator};
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source,
        file_check_frames,
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    let generator = app
        .world_mut()
        .spawn((
            ProcGenerator {
                mode: ProcGenMode::Anchored,
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(256.0, 10.0, 256.0),
                },
                seed: Some(11),
                tiles: Some(TileSettings {
                    cell_m: 64.0,
                    runtime: Some(AnchorSettings {
                        anchors: AnchorSource::Tagged,
                        base_reach_m: 40.0,
                        max_new_per_frame: Some(32),
                        ..AnchorSettings::default()
                    }),
                    ..TileSettings::default()
                }),
                ..ProcGenerator::new("s.pgraph.ron")
            },
            regia_scene::Transform::default(),
        ))
        .id();
    app.world_mut().spawn((
        ProcGenAnchor::default(),
        regia_scene::Transform::from_translation(Vec3::new(8.0, 0.0, 8.0)),
    ));
    (app, generator)
}

fn resident_cell(app: &mut regia_app::prelude::App) -> Option<regia_app::prelude::Entity> {
    use regia_app::prelude::*;
    use regia_procgen::tiles::ProcTileCell;
    let mut query = app.world_mut().query::<(Entity, &ProcTileCell)>();
    query.iter(app.world()).next().map(|(entity, _)| entity)
}

/// A second schedule of an unchanged memory graph does not read the graph text again.
#[test]
fn perf_g_second_schedule_does_not_reread_the_graph() {
    let _serial = serial();
    use regia_procgen::world::{ProcGraphSource, graph_text_reads};
    let (mut app, _) = perf_anchored(ProcGraphSource::Memory(perf_memory_files()), 30);
    app.update();
    let before = graph_text_reads();
    app.update();
    assert_eq!(
        graph_text_reads() - before,
        0,
        "the second schedule read the graph text"
    );
}

/// Changing a parameter still recompiles.
#[test]
fn perf_g_a_param_change_still_recompiles() {
    let _serial = serial();
    use regia_procgen::world::{ProcGenerator, ProcGraphSource, graph_text_reads};
    let (mut app, generator) = perf_anchored(ProcGraphSource::Memory(perf_memory_files()), 30);
    app.update();
    app.update();
    let before = graph_text_reads();
    app.world_mut()
        .get_mut::<ProcGenerator>(generator)
        .expect("generator")
        .params
        .insert("density".to_string(), "1.0".to_string());
    app.update();
    assert!(
        graph_text_reads() > before,
        "a parameter change did not read the graph"
    );
}

/// Deleting the graph and writing the same bytes back keeps the resident cell.
#[test]
fn perf_g_a_failed_reread_keeps_resident_cells() {
    let _serial = serial();
    use regia_procgen::tiles::ProcTileCell;
    use regia_procgen::world::ProcGraphSource;
    let dir = regia_test_utils::unique_temp_dir("procgen-perf-g-reread");
    let path = dir.join("s.pgraph.ron");
    let text = scatter_graph(8.0, 0.4, false).to_ron();
    let tmp = dir.join("s.pgraph.ron.tmp");
    std::fs::write(&tmp, &text).expect("write graph");
    std::fs::rename(&tmp, &path).expect("publish graph");
    let (mut app, _) = perf_anchored(ProcGraphSource::Dir(dir.clone()), 0);
    let mut cell = None;
    for _ in 0..8 {
        app.update();
        cell = resident_cell(&mut app);
        if cell.is_some() {
            break;
        }
    }
    let cell = cell.expect("a cell became resident");
    std::fs::remove_file(&path).expect("delete graph");
    app.update();
    std::fs::write(&tmp, &text).expect("restore graph");
    std::fs::rename(&tmp, &path).expect("publish restored graph");
    app.update();
    assert!(
        app.world().get::<ProcTileCell>(cell).is_some(),
        "a failed reread dropped the resident cell on the next good read"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Publishing a resident cell does not clone its [`RunOutput`].
#[test]
fn perf_g_publishing_a_cell_does_not_clone_the_output() {
    let _serial = serial();
    use regia_procgen::exec::run_output_clones;
    use regia_procgen::tiles::{ProcGenTileHost, ProcTileCell};
    use regia_procgen::world::{ProcGraphSource, ProcOutput};
    let (mut app, generator) = perf_anchored(ProcGraphSource::Memory(perf_memory_files()), 30);
    let before = run_output_clones();
    app.update();
    app.update();
    let clones = run_output_clones() - before;
    assert_eq!(clones, 0, "publishing the cell cloned RunOutput {clones} times");
    let cell = resident_cell(&mut app).expect("a cell became resident");
    let key = app.world().get::<ProcTileCell>(cell).expect("cell").key;
    let published = app.world().get::<ProcOutput>(cell).expect("published").0.clone();
    let shared = app
        .world()
        .get::<ProcGenTileHost>(generator)
        .and_then(|host| host.cell(key))
        .map(|cell| cell.output.clone())
        .expect("host cell");
    assert!(
        std::sync::Arc::ptr_eq(&published, &shared),
        "the published output is not the host cell's output"
    );
}

/// A second scheduler step with the same anchor does not rebuild the cell list.
#[test]
fn perf_g_second_step_reuses_the_wanted_set() {
    let _serial = serial();
    use regia_procgen::tiles::{
        AnchorSettings, AnchorView, ApplyBudget, GeneratorTiles, SchedTier, cells_in_rect_calls,
    };
    let mut tiles = GeneratorTiles::new(
        [-10_000.0, -10_000.0, 10_000.0, 10_000.0],
        vec![SchedTier {
            index: 0,
            cell_m: Some(16.0),
        }],
    );
    let settings = AnchorSettings {
        base_reach_m: 512.0,
        ..AnchorSettings::default()
    };
    let mut budget = ApplyBudget::default();
    let anchor = AnchorView::at(Vec3::ZERO);
    tiles.step(std::slice::from_ref(&anchor), &settings, &mut budget);
    let before = cells_in_rect_calls();
    tiles.step(std::slice::from_ref(&anchor), &settings, &mut budget);
    assert_eq!(
        cells_in_rect_calls() - before,
        0,
        "the second step rebuilt the cell list"
    );
    let moved = AnchorView::at(Vec3::new(1.0, 0.0, 0.0));
    tiles.step(std::slice::from_ref(&moved), &settings, &mut budget);
    assert!(
        cells_in_rect_calls() > before,
        "moving the anchor did not rebuild the cell list"
    );
}

/// A parameter that does not parse keeps the resident cells, is reported by `ProcGenTiles::errors`
/// (what `procgen.tiles` returns) on every later update, and is not recompiled each update while
/// nothing changes; fixing the parameter clears the error (corr-procgen: the error was never
/// reported and the same failure recompiled every update).
#[test]
fn a_compile_failure_on_a_resident_host_is_reported_once_and_not_recompiled() {
    let _serial = serial();
    use regia_procgen::tiles::ProcGenTiles;
    use regia_procgen::world::{ProcGenerator, ProcGraphSource, graph_text_reads};
    let (mut app, generator) = perf_anchored(ProcGraphSource::Memory(perf_memory_files()), 30);
    let mut cell = None;
    for _ in 0..8 {
        app.update();
        cell = resident_cell(&mut app);
        if cell.is_some() {
            break;
        }
    }
    let cell = cell.expect("a cell became resident");
    let set_param = |app: &mut regia_app::prelude::App, value: Option<&str>| {
        let mut g = app
            .world_mut()
            .get_mut::<ProcGenerator>(generator)
            .expect("generator");
        match value {
            Some(v) => g.params.insert("broken".to_string(), v.to_string()),
            None => g.params.remove("broken"),
        };
    };
    set_param(&mut app, Some("(("));
    app.update();
    let error = |app: &regia_app::prelude::App| {
        app.world()
            .resource::<ProcGenTiles>()
            .errors
            .get(&generator)
            .map(|(code, _)| code.clone())
    };
    assert_eq!(error(&app).as_deref(), Some("procgen_param_type"), "first update");
    let before = graph_text_reads();
    for _ in 0..5 {
        app.update();
        assert_eq!(
            error(&app).as_deref(),
            Some("procgen_param_type"),
            "the error stays reported while the parameter is broken"
        );
    }
    assert_eq!(
        graph_text_reads() - before,
        0,
        "an unchanged failing parameter was recompiled"
    );
    assert!(
        app.world()
            .get::<regia_procgen::tiles::ProcTileCell>(cell)
            .is_some(),
        "the failure dropped the resident cell"
    );
    set_param(&mut app, None);
    app.update();
    assert_eq!(error(&app), None, "a fixed parameter clears the error");
}

/// A graph file edited into a broken state is reported and is not recompiled on every file check
/// while it stays broken; reverting it to the last good text reads as a change, recompiles and
/// clears the error (fixrev2-procgen-k: a failed compile kept the last good stamps, so an
/// unchanged broken file recompiled every `file_check_frames` and a revert read as `Keep`, leaving
/// the stale error forever). This makes the "reported once and not recompiled" name true for file
/// failures as well as parameter failures.
#[test]
fn a_broken_graph_file_is_not_recompiled_and_its_error_clears_on_a_revert() {
    let _serial = serial();
    use regia_procgen::tiles::{ProcGenTiles, ProcTileCell};
    use regia_procgen::world::{ProcGenSettings, ProcGraphSource, graph_text_reads};
    let (mut app, generator) = perf_anchored(ProcGraphSource::Memory(perf_memory_files()), 0);
    let mut cell = None;
    for _ in 0..8 {
        app.update();
        cell = resident_cell(&mut app);
        if cell.is_some() {
            break;
        }
    }
    let cell = cell.expect("a cell became resident");
    let good_text = scatter_graph(8.0, 0.4, false).to_ron();
    let set_graph = |app: &mut regia_app::prelude::App, text: &str| {
        let mut settings = app.world_mut().resource_mut::<ProcGenSettings>();
        let ProcGraphSource::Memory(files) = &mut settings.source else {
            panic!("memory source");
        };
        files.insert("s.pgraph.ron".to_string(), text.to_string());
    };
    let error = |app: &regia_app::prelude::App| {
        app.world()
            .resource::<ProcGenTiles>()
            .errors
            .get(&generator)
            .map(|(code, _)| code.clone())
    };
    set_graph(&mut app, "((");
    app.update();
    assert!(
        error(&app).is_some(),
        "the broken graph is not reported on the update that sees it"
    );
    let before = graph_text_reads();
    for _ in 0..5 {
        app.update();
        assert!(
            error(&app).is_some(),
            "the error is not reported while the file is broken"
        );
    }
    assert_eq!(
        graph_text_reads() - before,
        0,
        "an unchanged broken file was recompiled"
    );
    set_graph(&mut app, &good_text);
    app.update();
    assert_eq!(
        error(&app),
        None,
        "reverting to the last good text did not clear the error"
    );
    assert!(
        app.world().get::<ProcTileCell>(cell).is_some(),
        "the revert dropped the resident cell"
    );
}

/// An unsaved graph edit followed by a run request reaches an anchored generator on the next update,
/// not at the next periodic file check (`ProcGraphOverlay`: "generators run the edited graph at
/// once"; corr-procgen: the re-queued cells ran the old graph for up to `file_check_frames`).
#[test]
fn an_overlay_edit_and_a_run_request_recompile_on_the_next_update() {
    let _serial = serial();
    use regia_procgen::world::{ProcGraphOverlay, ProcGraphSource, graph_text_reads, request_run};
    let (mut app, generator) = perf_anchored(ProcGraphSource::Memory(perf_memory_files()), 30);
    let mut resident = false;
    for _ in 0..8 {
        app.update();
        if resident_cell(&mut app).is_some() {
            resident = true;
            break;
        }
    }
    assert!(resident, "a cell became resident");
    app.world_mut().init_resource::<ProcGraphOverlay>();
    app.world_mut().resource_mut::<ProcGraphOverlay>().0.insert(
        "s.pgraph.ron".to_string(),
        scatter_graph(16.0, 0.4, false).to_ron(),
    );
    assert!(request_run(app.world_mut(), generator));
    let before = graph_text_reads();
    app.update();
    assert!(
        graph_text_reads() > before,
        "the edited graph was not compiled on the update after the run request"
    );
}

/// A resident anchored host on a memory source, checking its files every update.
fn resident_memory_host() -> (regia_app::prelude::App, regia_app::prelude::Entity) {
    use regia_procgen::world::ProcGraphSource;
    let (mut app, generator) = perf_anchored(ProcGraphSource::Memory(perf_memory_files()), 0);
    let mut resident = false;
    for _ in 0..8 {
        app.update();
        if resident_cell(&mut app).is_some() {
            resident = true;
            break;
        }
    }
    assert!(resident, "a cell became resident");
    (app, generator)
}

/// Writes one file of the memory source.
fn set_memory_file(app: &mut regia_app::prelude::App, path: &str, text: &str) {
    use regia_procgen::world::{ProcGenSettings, ProcGraphSource};
    let mut settings = app.world_mut().resource_mut::<ProcGenSettings>();
    let ProcGraphSource::Memory(files) = &mut settings.source else {
        panic!("memory source");
    };
    files.insert(path.to_string(), text.to_string());
}

/// The error `procgen.tiles` reports for `generator`.
fn host_error(
    app: &regia_app::prelude::App,
    generator: regia_app::prelude::Entity,
) -> Option<(String, String)> {
    app.world()
        .resource::<regia_procgen::tiles::ProcGenTiles>()
        .errors
        .get(&generator)
        .cloned()
}

/// `gen.grid → graph.output "out"`: a callable subgraph with one points output.
fn callee_graph() -> ProcGraph {
    use regia_procgen::graph::GraphPin;
    use regia_procgen::{DataKind, KindSet};
    let mut callee = graph(
        vec![
            (1, node("gen.grid", &[("count", "(2.0, 1.0, 1.0)")])),
            (2, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in")],
    );
    callee.outputs = vec![GraphPin {
        name: "out".to_string(),
        kinds: KindSet::of(&[DataKind::Points]),
    }];
    callee
}

/// The scatter graph plus a `graph.call` of `m.pgraph.ron`.
fn caller_graph() -> ProcGraph {
    let mut caller = scatter_graph(8.0, 0.4, false);
    caller
        .nodes
        .insert(10, node("graph.call", &[("graph", "\"m.pgraph.ron\"")]));
    caller
}

/// A graph edit that calls a subgraph that does not exist yet fails to compile; once the called
/// file appears, a file check recompiles and clears the error (rev-kimi-5 finding 1 on
/// fixrev2-procgen-k: a failed compile stamped only the last good compile's files, so the new,
/// missing path was never watched and the error stayed until `s.pgraph.ron` was touched again).
#[test]
fn rev_kimi5_a_called_graph_that_appears_later_clears_the_error() {
    let _serial = serial();
    use regia_procgen::ProcNodeRegistry;
    let (mut app, generator) = resident_memory_host();
    let caller = caller_graph();
    let callee = callee_graph();
    // Control: with the callee present the caller compiles.
    let both: BTreeMap<String, String> = [("m.pgraph.ron".to_string(), callee.to_ron())].into();
    caller
        .compile_full(&ProcNodeRegistry::with_builtins(), &BTreeMap::new(), &|p| {
            both.get(p).cloned()
        })
        .expect("the caller compiles once the callee exists");
    set_memory_file(&mut app, "s.pgraph.ron", &caller.to_ron());
    app.update();
    let first = host_error(&app, generator);
    assert!(first.is_some(), "a call of a missing graph does not compile");
    set_memory_file(&mut app, "m.pgraph.ron", &callee.to_ron());
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        host_error(&app, generator),
        None,
        "the called graph now exists, but the error {first:?} stayed"
    );
}

/// A newly called subgraph that appears broken: its parse error replaces the "cannot read" error,
/// the unchanged broken subgraph is not recompiled, and fixing it clears the error.
#[test]
fn a_broken_new_subgraph_is_watched_until_it_is_fixed() {
    let _serial = serial();
    use regia_procgen::world::graph_text_reads;
    let (mut app, generator) = resident_memory_host();
    set_memory_file(&mut app, "s.pgraph.ron", &caller_graph().to_ron());
    app.update();
    let missing = host_error(&app, generator);
    assert!(missing.is_some(), "a call of a missing graph does not compile");
    set_memory_file(&mut app, "m.pgraph.ron", "((");
    app.update();
    let broken = host_error(&app, generator);
    assert!(
        broken.is_some() && broken != missing,
        "the broken subgraph's error {broken:?} did not replace {missing:?}"
    );
    let before = graph_text_reads();
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        graph_text_reads() - before,
        0,
        "an unchanged broken subgraph was recompiled"
    );
    assert_eq!(
        host_error(&app, generator),
        broken,
        "the error stays while the subgraph is broken"
    );
    set_memory_file(&mut app, "m.pgraph.ron", &callee_graph().to_ron());
    app.update();
    assert_eq!(
        host_error(&app, generator),
        None,
        "the fixed subgraph did not clear the error {broken:?}"
    );
}

/// `ProcGraphVariant(parent: <parent>)` text with no overrides.
fn variant_text(parent: &str) -> String {
    use regia_procgen::graph::{PGVAR_FORMAT_VERSION, ProcGraphVariant};
    ProcGraphVariant {
        format_version: PGVAR_FORMAT_VERSION,
        parent: parent.to_string(),
        overrides: BTreeMap::new(),
    }
    .to_ron()
}

/// A resident anchored host on `v.pgvar.ron`, a variant of `s.pgraph.ron`, checking its files every
/// update.
fn resident_variant_host() -> (regia_app::prelude::App, regia_app::prelude::Entity) {
    use regia_procgen::world::{ProcGenerator, ProcGraphSource};
    let mut files = perf_memory_files();
    files.insert("v.pgvar.ron".to_string(), variant_text("s.pgraph.ron"));
    let (mut app, generator) = perf_anchored(ProcGraphSource::Memory(files), 0);
    "v.pgvar.ron".clone_into(
        &mut app
            .world_mut()
            .get_mut::<ProcGenerator>(generator)
            .expect("generator")
            .graph,
    );
    let mut resident = false;
    for _ in 0..8 {
        app.update();
        if resident_cell(&mut app).is_some() {
            resident = true;
            break;
        }
    }
    assert!(resident, "a cell of the variant became resident");
    assert_eq!(host_error(&app, generator), None, "the variant compiles");
    (app, generator)
}

/// The variant/base twin: the variant is edited to a base file that does not exist yet. The
/// failure is not recompiled while the base stays missing, and creating the base recompiles and
/// clears the error.
#[test]
fn a_missing_variant_base_that_appears_later_clears_the_error() {
    let _serial = serial();
    use regia_procgen::tiles::ProcTileCell;
    use regia_procgen::world::graph_text_reads;
    let (mut app, generator) = resident_variant_host();
    let cell = resident_cell(&mut app).expect("resident cell");
    set_memory_file(&mut app, "v.pgvar.ron", &variant_text("b.pgraph.ron"));
    app.update();
    let first = host_error(&app, generator);
    assert!(
        first
            .as_ref()
            .is_some_and(|(code, _)| code == "procgen_unknown_graph"),
        "a variant of a missing base reports procgen_unknown_graph, got {first:?}"
    );
    let before = graph_text_reads();
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        graph_text_reads() - before,
        0,
        "an unchanged failure with a still-missing base was recompiled"
    );
    assert_eq!(
        host_error(&app, generator),
        first,
        "the error stays while the base is missing"
    );
    set_memory_file(&mut app, "b.pgraph.ron", &scatter_graph(8.0, 0.4, false).to_ron());
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        host_error(&app, generator),
        None,
        "the base file now exists, but the error {first:?} stayed"
    );
    assert!(
        app.world().get::<ProcTileCell>(cell).is_some(),
        "the new base holds the same graph: the resident cell stays"
    );
}

/// Renaming a variant's base (the variant now names the new file, the old one is deleted) compiles
/// the edit: a gone file no longer blocks a changed one (fixrev5-procgen-missing-calls: the failed
/// reread of the deleted base won over the changed variant, so the edit was never compiled while
/// the old base stayed deleted, and the cells kept the old graph without an error).
#[test]
fn a_variant_repointed_at_a_renamed_base_is_compiled() {
    let _serial = serial();
    use regia_procgen::world::{ProcGenSettings, ProcGraphSource, graph_text_reads};
    let (mut app, generator) = resident_variant_host();
    set_memory_file(
        &mut app,
        "n.pgraph.ron",
        &scatter_graph(16.0, 0.4, false).to_ron(),
    );
    set_memory_file(&mut app, "v.pgvar.ron", &variant_text("n.pgraph.ron"));
    {
        let mut settings = app.world_mut().resource_mut::<ProcGenSettings>();
        let ProcGraphSource::Memory(files) = &mut settings.source else {
            panic!("memory source");
        };
        files.remove("s.pgraph.ron");
    }
    let before = graph_text_reads();
    for _ in 0..5 {
        app.update();
    }
    assert!(
        graph_text_reads() > before,
        "the repointed variant was never compiled while its old base stayed deleted"
    );
    assert_eq!(
        host_error(&app, generator),
        None,
        "the repointed variant compiles"
    );
}
