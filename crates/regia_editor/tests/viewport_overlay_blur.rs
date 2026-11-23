// SPDX-License-Identifier: MPL-2.0
//! The viewport's frosted overlay chips through the real editor window (ADR-2129): off by default
//! with nothing of it running, and when turned on, the stats block and the view label are blurred
//! while the rest of the picture stays. Needs a software adapter (lavapipe).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::command::{EditorCommand as _, SpawnEntities, UndoStack};
use regia_editor::selection::Selection;
use regia_editor::shell::EditorShell;
use regia_editor::viewport::ViewportSettings;
use regia_editor::viewport_blur::ViewportBlurState;
use regia_render::MeshSource;
use regia_scene::prelude::Transform;

fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

fn spawn(app: &mut App, name: &str, transform: Transform) -> Entity {
    let mut command =
        SpawnEntities::new_entity(app.world(), Guid::new_random(), name, transform).expect("spawn command");
    command.apply(app.world_mut());
    let world = app.world();
    let entity = world
        .iter_entities()
        .map(|e| e.id())
        .find(|e| regia_editor::outliner::display_name(world, *e) == name)
        .expect("spawned");
    app.world_mut()
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    entity
}

fn capture(app: &mut App, shell: &mut EditorShell, device: &regia_render::RenderDevice) -> egui::ColorImage {
    let ctx = egui::Context::default();
    regia_editor::window_capture::capture_editor_window(
        shell,
        &ctx,
        app.world_mut(),
        device,
        [1280, 800],
        1.0,
        3,
    )
    .expect("window capture")
}

#[test]
fn overlay_blur_is_off_by_default_and_blurs_the_chips_when_on() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let parent = regia_test_utils::unique_temp_dir("editor-overlay-blur");
    regia_editor::project::set_recent_file_for_tests(parent.join("recents.txt"));
    let mut app = edit_app();
    regia_editor::project::create_and_open(app.world_mut(), "Blur Yard", &parent, "blank").expect("project");
    let root = app
        .world()
        .get_resource::<regia_editor::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .expect("project root");
    let cube = spawn(
        &mut app,
        "Cube",
        Transform::from_translation(Vec3::new(4.0, 1.0, 0.0)),
    );
    app.world_mut()
        .entity_mut(cube)
        .insert(MeshSource::new(MeshSource::BUILTIN_CUBE, ""));
    let mut layout = regia_editor::views::ViewportLayout::default();
    layout.show.stats = true;
    app.world_mut().insert_resource(layout);
    app.update();

    let mut shell = EditorShell::new(Some(device.clone()), Some(root));
    assert_eq!(ViewportSettings::default().overlay_blur, 0.0);
    let off = capture(&mut app, &mut shell, &device);
    assert!(
        app.world().get_resource::<ViewportBlurState>().is_none(),
        "with the setting at zero nothing of the blur runs"
    );

    app.world_mut().resource_mut::<ViewportSettings>().overlay_blur = 8.0;
    let on = capture(&mut app, &mut shell, &device);
    let state = app.world().resource::<ViewportBlurState>();
    assert_eq!(state.chips, 2, "the stats block and the view label");

    // Far from the chips the picture is what it was: the middle of the window holds the viewport.
    let (w, h) = (1280usize, 800usize);
    let (mut sum, mut count) = (0u64, 0u64);
    for y in h * 2 / 5..h * 3 / 5 {
        for x in w * 2 / 5..w * 3 / 5 {
            let (a, b) = (off.pixels[y * w + x], on.pixels[y * w + x]);
            sum += u64::from(a.r().abs_diff(b.r()))
                + u64::from(a.g().abs_diff(b.g()))
                + u64::from(a.b().abs_diff(b.b()));
            count += 3;
        }
    }
    let mean = sum as f64 / count as f64;
    assert!(mean < 2.0, "the viewport's middle changed by {mean}");

    let _ = std::fs::remove_dir_all(&parent);
}
