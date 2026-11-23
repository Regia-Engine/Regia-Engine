// SPDX-License-Identifier: MPL-2.0
//! First-use hitches (ADR-8584, the w12 all-host smoke bugs 16-18): opening the Material Graph,
//! the Anim Graph or selecting a new camera must not compile a pipeline in the UI frame. Each test
//! reads `regia_render::pipeline_cache::pipelines_created_on_this_thread` (the test thread is the
//! UI thread here) around the frame that opens the panel, then waits for the preview thread and
//! checks the preview lands within a fixed number of frames. Times are printed for the report
//! (`FIRST_USE` lines, `--nocapture`), never asserted. Lavapipe or a GPU; skipped without one.

use std::path::Path;

use regia_ecs::prelude::*;

use crate::canvas_keys::test_input::frame;
use crate::command::UndoStack;

/// Frames after the preview thread went idle within which a preview must show.
const FRAMES_AFTER_WORKER: usize = 3;

const SIZE: [f32; 2] = [1400.0, 1600.0];

/// A graph with a few nodes (so the node swatches have work) and a value no other test uses (so
/// its forward pipeline is new in this process).
const GRAPH: &str = r#"(
    format_version: 5,
    name: "first_use",
    params: [(name: "tint", value: Color((0.31, 0.62, 0.17)))],
    nodes: {
        "tint": Param("tint"),
        "k": Float(0.4375),
        "rough": Mul("k", "k"),
        "glow": Mul("tint", "k"),
    },
    output: (base_color: Some("tint"), roughness: Some("rough"), emissive: Some("glow")),
)"#;

fn device() -> Option<regia_render::RenderDevice> {
    match crate::render::headless_device() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip: no headless device: {err}");
            None
        }
    }
}

fn ui_pipelines() -> u64 {
    regia_render::pipeline_cache::pipelines_created_on_this_thread()
}

/// Runs one frame of `draw` and returns (pipelines created on this thread, milliseconds).
fn measured(ctx: &egui::Context, draw: impl FnMut(&mut egui::Ui)) -> (u64, f64) {
    let before = ui_pipelines();
    let started = std::time::Instant::now();
    frame(ctx, SIZE, vec![], draw);
    (ui_pipelines() - before, started.elapsed().as_secs_f64() * 1000.0)
}

/// Waits for the preview thread, returning its wait in milliseconds.
fn wait_worker() -> f64 {
    let started = std::time::Instant::now();
    crate::preview_worker::wait_idle();
    started.elapsed().as_secs_f64() * 1000.0
}

fn write(path: &Path, body: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("dir");
    }
    std::fs::write(path, body).expect("write");
}

#[test]
fn opening_the_material_graph_creates_no_pipeline_on_the_ui_thread() {
    use crate::matgraph_editor::MatGraphEditor;
    let Some(device) = device() else { return };
    let root = regia_test_utils::unique_temp_dir("first-use-matgraph");
    let file = root.join("assets/first_use.matgraph.ron");
    write(&file, GRAPH);
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    let mut editor = MatGraphEditor::default();
    editor.root = Some(root.clone());
    editor.open(&file).expect("open");
    editor.ui.node_previews = true;
    world.insert_resource(editor);
    let ctx = egui::Context::default();
    let panel = |world: &mut World, ui: &mut egui::Ui| {
        crate::matgraph_editor::panel::matgraph_editor_panel(ui, world, Some(&device))
    };

    let (created, open_ms) = measured(&ctx, |ui| panel(&mut world, ui));
    let worker_ms = wait_worker();
    eprintln!(
        "FIRST_USE matgraph open_frame_ms={open_ms:.1} ui_pipelines={created} worker_ms={worker_ms:.1}"
    );
    assert_eq!(
        created, 0,
        "the frame that opens the Material Graph compiled {created} pipelines"
    );

    let mut shown = None;
    for n in 1..=FRAMES_AFTER_WORKER {
        let (created, ms) = measured(&ctx, |ui| panel(&mut world, ui));
        eprintln!("FIRST_USE matgraph frame+{n}_ms={ms:.1} ui_pipelines={created}");
        assert_eq!(
            created, 0,
            "frame {n} after the worker compiled {created} pipelines"
        );
        if world.resource::<MatGraphEditor>().ui.preview.is_some() {
            shown = Some(n);
            break;
        }
        wait_worker();
    }
    assert!(
        shown.is_some(),
        "the preview shows within {FRAMES_AFTER_WORKER} frames after the preview thread finished"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn opening_the_anim_graph_creates_no_pipeline_on_the_ui_thread() {
    let Some(device) = device() else { return };
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    let ctx = egui::Context::default();
    let panel = |world: &mut World, ui: &mut egui::Ui| {
        crate::anim::graph_panel::anim_graph_panel(ui, world, Some(&device))
    };

    let (created, open_ms) = measured(&ctx, |ui| panel(&mut world, ui));
    let worker_ms = wait_worker();
    eprintln!(
        "FIRST_USE anim_graph open_frame_ms={open_ms:.1} ui_pipelines={created} worker_ms={worker_ms:.1}"
    );
    assert_eq!(
        created, 0,
        "the frame that opens the Anim Graph compiled {created} pipelines"
    );

    let mut shown = None;
    for n in 1..=FRAMES_AFTER_WORKER {
        let (created, ms) = measured(&ctx, |ui| panel(&mut world, ui));
        eprintln!("FIRST_USE anim_graph frame+{n}_ms={ms:.1} ui_pipelines={created}");
        assert_eq!(
            created, 0,
            "frame {n} after the worker compiled {created} pipelines"
        );
        if crate::anim::preview::rig_preview_texture(&ctx, "anim-graph-preview").is_some() {
            shown = Some(n);
            break;
        }
        wait_worker();
    }
    assert!(
        shown.is_some(),
        "the rig preview shows within {FRAMES_AFTER_WORKER} frames after the preview thread finished"
    );
}

#[test]
fn creating_a_camera_creates_no_pipeline_on_the_ui_thread() {
    let Some(device) = device() else { return };
    let mut world = World::new();
    world.insert_resource(crate::viewport::EditorCamera::default());
    world.insert_resource(UndoStack::new());
    let camera = world
        .spawn((
            regia_scene::prelude::Transform {
                translation: regia_core::math::Vec3::new(0.0, 1.0, 6.0),
                ..Default::default()
            },
            regia_render::Camera::default(),
        ))
        .id();
    let mut selection = crate::selection::Selection::default();
    selection.select(camera, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
    let ctx = egui::Context::default();
    let draw = |world: &mut World, ui: &mut egui::Ui| {
        let pane = ui.max_rect();
        crate::camera_preview::draw(ui, world, Some(&device), pane);
    };
    let texture = |ctx: &egui::Context| {
        ctx.data(|d| d.get_temp::<egui::TextureHandle>(egui::Id::new("regia_editor::camera-preview-texture")))
    };

    let (created, open_ms) = measured(&ctx, |ui| draw(&mut world, ui));
    let worker_ms = wait_worker();
    eprintln!("FIRST_USE camera open_frame_ms={open_ms:.1} ui_pipelines={created} worker_ms={worker_ms:.1}");
    assert_eq!(
        created, 0,
        "the frame that selects the new camera compiled {created} pipelines"
    );

    let mut shown = None;
    for n in 1..=FRAMES_AFTER_WORKER {
        let (created, ms) = measured(&ctx, |ui| draw(&mut world, ui));
        eprintln!("FIRST_USE camera frame+{n}_ms={ms:.1} ui_pipelines={created}");
        if texture(&ctx).is_some() {
            shown = Some(n);
            break;
        }
        wait_worker();
    }
    assert!(
        shown.is_some(),
        "the camera preview shows within {FRAMES_AFTER_WORKER} frames after the preview thread finished"
    );
}
