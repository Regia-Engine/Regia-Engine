// SPDX-License-Identifier: MPL-2.0
//! Per-frame clones in the Inspector and the Layers panel (lane v06-editor-usability-a item 3, the
//! w11 planner's "Inspector reflect-clones a whole component per frame and the Layers panel clones
//! the level per frame"), measured with an allocation counter, not wall time.
//!
//! A steady repaint of either panel must not allocate in proportion to the data behind it: the
//! allocations of one steady frame are compared between a small and a large component (the
//! Inspector, on a prefab member, whose field tools compare against the prefab's value) and a
//! small and a large open level (the Layers panel). egui itself allocates every frame, so zero is
//! not the bar; growth with the data is the regression. This binary holds one test so the
//! counting allocator sees one thread's work only.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_editor::command::UndoStack;
use regia_editor::selection::{SelectMode, Selection};
use regia_scene::prefab::PrefabLink;
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

/// A component whose clone costs one allocation per line.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Notes {
    lines: Vec<String>,
}

fn notes(n: usize) -> Notes {
    Notes {
        lines: (0..n).map(|i| format!("line {i}")).collect(),
    }
}

/// An editor world with a project and a new open level.
fn app(label: &str) -> (App, std::path::PathBuf) {
    let dir = regia_test_utils::unique_temp_dir(label);
    std::fs::create_dir_all(dir.join("assets")).expect("assets");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<Notes>();
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.insert_resource(UndoStack::new());
    (app, dir)
}

/// The allocations of the cheapest of four steady frames of `draw`, after four warm-up frames.
fn steady_allocs(world: &mut World, draw: fn(&mut egui::Ui, &mut World)) -> u64 {
    let ctx = egui::Context::default();
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    let mut best = u64::MAX;
    for n in 0..8 {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 1400.0),
            )),
            time: Some(n as f64 / 60.0),
            predicted_dt: 1.0 / 60.0,
            ..Default::default()
        };
        let before = ALLOCS.with(Cell::get);
        let mut out = ctx.run_ui(raw, |ui| draw(ui, world));
        out.textures_delta.clear();
        drop(out);
        let spent = ALLOCS.with(Cell::get) - before;
        if n >= 4 {
            best = best.min(spent);
        }
    }
    best
}

/// One steady Inspector frame's allocations with a prefab member holding `n` lines selected.
fn inspector_allocs(n: usize) -> u64 {
    let (mut app, dir) = app(&format!("frame-clones-inspector-{n}"));
    let world = app.world_mut();
    let entity = world
        .spawn((
            EntityGuid(Guid::new_random()),
            Name::new("Board"),
            Transform::default(),
            notes(n),
        ))
        .id();
    regia_editor::prefab_create::create_prefab(
        world,
        &[entity],
        &dir.join("assets").join(format!("board{n}.prefab.ron")),
    )
    .expect("create prefab");
    let member = {
        let mut query = world.query_filtered::<Entity, (With<PrefabLink>, With<Notes>)>();
        query
            .iter(world)
            .next()
            .expect("the prefab member holds the notes")
    };
    // The field tools compare against the prefab's value: the path this test is about.
    let tools = regia_editor::inspector::clipboard::FieldTools::for_component(
        world,
        member,
        "Notes",
        world.get::<Notes>(member).expect("notes").as_partial_reflect(),
    );
    assert_eq!(tools.baseline_source, "prefab");
    let mut selection = Selection::default();
    selection.select(member, SelectMode::Replace);
    world.insert_resource(selection);
    let spent = steady_allocs(world, regia_editor::inspector::inspector_panel);
    drop(app);
    let _ = std::fs::remove_dir_all(dir);
    spent
}

/// One steady Layers panel frame's allocations with `n` entities in the open level.
fn layers_allocs(n: usize) -> u64 {
    let (mut app, dir) = app(&format!("frame-clones-layers-{n}"));
    let world = app.world_mut();
    let level = world
        .get_resource::<regia_editor::level_io::OpenLevel>()
        .expect("open level")
        .id;
    for i in 0..n {
        world.spawn((
            EntityGuid(Guid::new_random()),
            Name::new(format!("Rock {i}")),
            Transform::default(),
            regia_scene::level::LevelMember(level.0.as_u128()),
        ));
    }
    // A save puts the level into the library the panel reads its binding from.
    let open = world
        .get_resource::<regia_editor::level_io::OpenLevel>()
        .cloned()
        .expect("open level");
    let path = dir.join("assets").join("rocks.level.ron");
    regia_editor::level_io::save_level_file(world, &open, &path).expect("save");
    let reopened = regia_editor::level_io::open_level_file(world, &path).expect("reopen");
    world.insert_resource(reopened);
    app.update();
    let spent = steady_allocs(app.world_mut(), regia_editor::layers_panel::layers_panel);
    drop(app);
    let _ = std::fs::remove_dir_all(dir);
    spent
}

#[test]
fn steady_inspector_and_layers_frames_do_not_allocate_with_the_data_size() {
    let (small, large) = (inspector_allocs(16), inspector_allocs(4096));
    assert!(
        large < small + 512,
        "a steady Inspector frame on a prefab member allocated {small} times with 16 lines and \
         {large} times with 4096: the field tools clone the prefab's component every frame"
    );
    let (small, large) = (layers_allocs(16), layers_allocs(4096));
    assert!(
        large < small + 512,
        "a steady Layers panel frame allocated {small} times with 16 entities and {large} times \
         with 4096"
    );
}
