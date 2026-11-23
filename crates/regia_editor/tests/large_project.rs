// SPDX-License-Identifier: MPL-2.0
//! Large-project soak for the editor's egui panels (owner plan v0.6.0 Stage 2, "large projects",
//! ADR-7306): a synthetic project with 100 000 entities in a 6-deep hierarchy, 100 000 log lines, a
//! 100 000-element component list (plus a 20 000-entry map) and 50 000 assets on disk. Each panel is
//! drawn in a headless egui `Context` (CPU only, no GPU) and measured per frame:
//!
//! - **text shapes** the frame emits (deterministic: what was laid out),
//! - **allocations** per frame (a counting allocator; deterministic for a fixed input),
//! - **draw time**, as a ratio to a control panel drawn in the same run (a fixed 200-label list),
//!   so host load scales both sides and does not flake the bound.
//!
//! The bounds were measured on the base (lane/sprint-post05w4-editor-polish, ee721c603) and after
//! the virtualization; both sets of numbers are in `docs/releases/v0.1.28-v06-editor-scale.md`.
//! `--nocapture` prints them. This binary holds one test so the counting allocator and the timing
//! see one thread's work only.

#![allow(unsafe_code)] // the counting allocator below

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::content_index::ContentIndex;
use regia_editor::inspector;
use regia_editor::log_panel::{LogPanelState, output_log_panel};
use regia_editor::outliner::{OutlinerRows, outliner_panel};
use regia_scene::prelude::{EntityGuid, Name, Transform};

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: forwards to the system allocator unchanged; the counter is a thread-local `Cell`.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Entities in the synthetic level.
const ENTITIES: usize = 100_000;
/// Hierarchy depth (roots are depth 1).
const DEPTH: usize = 6;
/// Root entities; every other entity has `BRANCH` children until `ENTITIES` is reached.
const ROOTS: usize = 11;
/// Children per parent.
const BRANCH: usize = 6;
/// Log lines pushed through the engine's log ring.
const LOG_LINES: usize = 100_000;
/// Elements of the inspected component's list.
const LIST_LEN: usize = 100_000;
/// Entries of the inspected component's map.
const MAP_LEN: usize = 20_000;
/// Asset files in the synthetic project.
const ASSETS: usize = 50_000;

/// Frames drawn before measuring (egui settles its layout over two frames).
const WARM: usize = 3;
/// Frames measured.
const MEASURED: usize = 5;

/// A component with large containers (the inspector's worst case).
#[derive(Component, Reflect, Clone, Debug, Default)]
#[reflect(Component, Default)]
struct BigData {
    samples: Vec<u32>,
    table: BTreeMap<u32, f32>,
}

/// What one panel cost per frame.
#[derive(Debug, Clone, Copy)]
struct Cost {
    /// Median draw time.
    median: Duration,
    /// Mean allocations per measured frame.
    allocs: u64,
    /// Text shapes in the last frame.
    texts: usize,
}

fn count_texts(shapes: &[egui::epaint::ClippedShape]) -> usize {
    fn walk(shape: &egui::Shape) -> usize {
        match shape {
            egui::Shape::Text(_) => 1,
            egui::Shape::Vec(shapes) => shapes.iter().map(walk).sum(),
            _ => 0,
        }
    }
    shapes.iter().map(|s| walk(&s.shape)).sum()
}

/// One frame of `draw` in a 420 × 900 panel; returns (time, allocations, text shapes).
fn frame(ctx: &egui::Context, n: usize, draw: &mut dyn FnMut(&mut egui::Ui)) -> (Duration, u64, usize) {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(420.0, 900.0),
        )),
        time: Some(n as f64 / 60.0),
        predicted_dt: 1.0 / 60.0,
        ..Default::default()
    };
    let before = ALLOCS.with(Cell::get);
    let started = Instant::now();
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| draw(ui));
    });
    let elapsed = started.elapsed();
    let allocs = ALLOCS.with(Cell::get) - before;
    output.textures_delta.clear();
    (elapsed, allocs, count_texts(&output.shapes))
}

fn measure(name: &str, draw: &mut dyn FnMut(&mut egui::Ui)) -> Cost {
    let ctx = egui::Context::default();
    let mut n = 0;
    for _ in 0..WARM {
        frame(&ctx, n, draw);
        n += 1;
    }
    let mut times = Vec::new();
    let mut allocs = 0;
    let mut texts = 0;
    for _ in 0..MEASURED {
        let (t, a, x) = frame(&ctx, n, draw);
        n += 1;
        times.push(t);
        allocs += a;
        texts = x;
    }
    times.sort();
    let cost = Cost {
        median: times[times.len() / 2],
        allocs: allocs / MEASURED as u64,
        texts,
    };
    eprintln!(
        "[large_project] {name:<10} median {:>10.3} ms  allocs/frame {:>9}  text shapes {:>7}",
        cost.median.as_secs_f64() * 1000.0,
        cost.allocs,
        cost.texts
    );
    cost
}

/// The level: `ROOTS` roots, `BRANCH` children per parent, `DEPTH` levels, `ENTITIES` in all.
fn large_level() -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<BigData>();
    app.init_resource::<UndoStack>();
    let world = app.world_mut();
    let mut level: Vec<Entity> = (0..ROOTS)
        .map(|i| {
            world
                .spawn((
                    Name::new(format!("Root {i:02}")),
                    Transform::default(),
                    EntityGuid::new_random(),
                ))
                .id()
        })
        .collect();
    let mut total = level.len();
    let mut depth = 1;
    while total < ENTITIES && depth < DEPTH {
        let mut next = Vec::new();
        'parents: for parent in &level {
            for c in 0..BRANCH {
                if total == ENTITIES {
                    break 'parents;
                }
                let child = world
                    .spawn((
                        Name::new(format!("Node d{depth} {total:06} c{c}")),
                        Transform::default(),
                        EntityGuid::new_random(),
                        ChildOf(*parent),
                    ))
                    .id();
                next.push(child);
                total += 1;
            }
        }
        level = next;
        depth += 1;
    }
    assert_eq!(total, ENTITIES, "the generator fills the level");
    assert_eq!(depth, DEPTH, "six levels deep");
    world.spawn((
        Name::new("Big data"),
        Transform::default(),
        EntityGuid::new_random(),
        BigData {
            samples: (0..LIST_LEN as u32).collect(),
            table: (0..MAP_LEN as u32).map(|k| (k, k as f32 * 0.5)).collect(),
        },
    ));
    regia_editor::visibility::mark_internal_entities(world);
    app
}

/// The control panel: 200 plain labels, the same every frame.
fn control(ui: &mut egui::Ui) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        for i in 0..200 {
            ui.label(format!("control row {i}"));
        }
    });
}

fn ratio(cost: Cost, control: Cost) -> f64 {
    cost.median.as_secs_f64() / control.median.as_secs_f64().max(1e-9)
}

#[test]
fn large_project_panels_stay_within_their_bounds() {
    let mut app = large_level();
    let world = app.world_mut();
    let big = world
        .query::<(Entity, &BigData)>()
        .iter(world)
        .map(|(e, _)| e)
        .next()
        .expect("big data entity");

    let control_cost = measure("control", &mut control);

    // Outliner: every node open (the default), the panel scrolled to the top.
    let outliner = measure("outliner", &mut |ui| outliner_panel(ui, world));
    let rows = world.resource::<OutlinerRows>();
    let drawn = rows.rows.len();
    eprintln!(
        "[large_project] outliner rows drawn {drawn}, listed {}",
        rows.order().len()
    );
    assert_eq!(
        rows.order().len(),
        ENTITIES + 1,
        "every entity is listed (all open)"
    );
    assert_eq!(
        world
            .resource::<regia_editor::outliner::OutlinerView>()
            .tree_builds(),
        1,
        "the tree was built once for all the frames"
    );

    // Output Log: 100 000 lines through the ring, the panel polled between chunks as the shell does.
    let mut log = LogPanelState::default();
    log.min_level = String::new();
    {
        let ctx = egui::Context::default();
        let mut pushed = 0;
        let mut n = 0;
        while pushed < LOG_LINES {
            let chunk = (LOG_LINES - pushed).min(2_000);
            for i in pushed..pushed + chunk {
                regia_core::log::ring::push(
                    if i % 97 == 0 { "WARN" } else { "INFO" },
                    "large_project::soak",
                    format!(
                        "line {i:06}: loaded assets/props/prop_{:05}.glb in {} ms",
                        i % 50_000,
                        i % 17
                    ),
                );
            }
            pushed += chunk;
            frame(&ctx, n, &mut |ui| {
                let _ = output_log_panel(ui, &mut log);
            });
            n += 1;
        }
    }
    let log_cost = measure("log", &mut |ui| {
        let _ = output_log_panel(ui, &mut log);
    });
    assert!(
        log.kept() >= LOG_LINES,
        "the panel kept every line: {}",
        log.kept()
    );
    assert!(log.shown() >= LOG_LINES, "and lists them: {}", log.shown());
    assert!(
        log.tested() <= 2 * (LOG_LINES as u64 + regia_core::log::ring::CAPACITY as u64),
        "each line was tested about once, not once per frame: {}",
        log.tested()
    );

    // Inspector: the big component's list and map.
    let inspector_cost = measure("inspector", &mut |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let _ = inspector::component_ui(ui, world, big, "BigData", &[]);
        });
    });

    eprintln!(
        "[large_project] time / control: outliner {:.1}x, log {:.1}x, inspector {:.1}x",
        ratio(outliner, control_cost),
        ratio(log_cost, control_cost),
        ratio(inspector_cost, control_cost)
    );

    // Content index: a full scan of 50 000 assets on disk (timed and printed, counted below).
    let root = regia_test_utils::unique_temp_dir("large_project_assets");
    for i in 0..ASSETS {
        let ext = if i % 10 == 0 { "png" } else { "glb" };
        let path = root.join(format!("assets/set_{:03}/prop_{i:05}.{ext}", i / 100));
        if i % 100 == 0 {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        }
        std::fs::write(&path, b"").expect("write asset");
    }
    let mut index = ContentIndex::default();
    let started = Instant::now();
    index.ensure(Some(&root));
    let scan = started.elapsed();
    eprintln!(
        "[large_project] content index full scan of {} assets: {:.1} ms ({} dirs read)",
        index.assets().len(),
        scan.as_secs_f64() * 1000.0,
        index.dirs_read
    );
    assert_eq!(index.assets().len(), ASSETS, "every asset indexed");
    assert_eq!(index.scans, 1, "one walk");
    let _ = std::fs::remove_dir_all(&root);

    // --- Bounds (ADR-7306): see the module docs for where the numbers come from. ---
    assert_bounds(drawn, outliner, log_cost, inspector_cost, control_cost);
}

/// The bounds (ADR-7306). Measured on the build server (debug test profile, load ~150):
///
/// | panel | base ee721c603: time / control, allocs per frame | after: time / control, allocs per frame |
/// |---|---|---|
/// | outliner | 15 671x, 4 300 316 (100 001 rows laid out) | 30.6x, 1 359 (40 rows) |
/// | log | 79.9x, 14 062 | 1.9x, 1 105 |
/// | inspector | 21 569x, 3 603 432 | 96.7x, 148 832 |
///
/// The time bounds are ten times the after ratio or more, and still far under the base; the
/// allocation bounds are about three times the after count. The inspector's allocations were
/// mostly the per-frame reflect clone of the whole component until ADR-7326 kept the copy between
/// repaints (the bounds were not tightened: numbers only, see the w5-editor-scale-followups report).
fn assert_bounds(drawn: usize, outliner: Cost, log: Cost, inspector: Cost, control: Cost) {
    assert!(drawn <= 64, "the outliner lays out a screenful, not {drawn} rows");
    assert!(
        outliner.allocs <= 5_000,
        "outliner allocations per frame: {}",
        outliner.allocs
    );
    assert!(log.allocs <= 5_000, "log allocations per frame: {}", log.allocs);
    assert!(
        inspector.allocs <= 450_000,
        "inspector allocations per frame: {}",
        inspector.allocs
    );
    assert!(
        ratio(outliner, control) <= 400.0,
        "outliner time / control: {:.1}",
        ratio(outliner, control)
    );
    assert!(
        ratio(log, control) <= 25.0,
        "log time / control: {:.1}",
        ratio(log, control)
    );
    assert!(
        ratio(inspector, control) <= 1_500.0,
        "inspector time / control: {:.1}",
        ratio(inspector, control)
    );
}
