// SPDX-License-Identifier: MPL-2.0
//! Create ▸ Shape end to end: shapes made through the editor's create path render as their real
//! meshes and colours in the viewport (not as grey proxy cubes), and save with the level.

use regia_core::math::Vec3;
use regia_editor::render::render_viewport;
use regia_editor::selection::Selection;
use regia_editor::shapes::create_shape;
use regia_editor::viewport::{self, EditorCamera};
use regia_render::{Primitive, PrimitiveShape};
use regia_scene::prelude::Transform;

fn edit_world() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    // As the editor's main does: shapes are level content.
    app.register_type::<Primitive>();
    app.world_mut().insert_resource(EditorCamera::default());
    app
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn created_shapes_render_their_own_mesh_and_colour() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let mut app = edit_world();
    let world = app.world_mut();
    let kinds = ["sphere", "capsule", "cone", "torus", "ramp"];
    for (i, kind) in kinds.iter().enumerate() {
        let entity = create_shape(world, kind).expect("created");
        if let Some(mut transform) = world.get_mut::<Transform>(entity) {
            transform.translation = Vec3::new(i as f32 * 1.6 - 3.2, 0.6, 0.0);
        }
        if let Some(mut primitive) = world.get_mut::<Primitive>(entity) {
            // Pure red so the viewport's own colour is measurable (proxies are grey).
            primitive.color = [0.9, 0.05, 0.05, 1.0];
        }
    }
    // Nothing selected: every shape draws in its own colour, not the highlight.
    world.insert_resource(Selection::default());

    let camera = EditorCamera {
        target: Vec3::new(0.0, 0.5, 0.0),
        distance: 9.0,
        ..EditorCamera::default()
    };
    let image = render_viewport(&device, world, &camera, [480.0, 270.0]).expect("viewport render");
    let report = viewport::measure_viewport(&image);
    assert!(report.sky_visible(), "{report:?}");
    assert!(report.shading_visible(), "{report:?}");
    let red = image
        .pixels
        .iter()
        .filter(|p| u32::from(p.r()) > u32::from(p.g()) + 60 && u32::from(p.r()) > u32::from(p.b()) + 60)
        .count();
    assert!(
        red > 2_000,
        "the red primitives show their own color ({red} red pixels)"
    );

    if let Ok(dir) = regia_core::brand::env_var("CARGO_TARGET_TMPDIR") {
        let path = std::path::Path::new(&dir).join("viewport_shapes.png");
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
        if let Some(img) = image::RgbaImage::from_raw(480, 270, rgba) {
            let _ = img.save(&path);
            eprintln!("viewport written to {}", path.display());
        }
    }
}

#[test]
fn created_shapes_survive_a_level_round_trip() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = edit_world();
    let world = app.world_mut();
    let entity = create_shape(world, "prism").expect("created");
    let shape = world
        .get::<Primitive>(entity)
        .map(|p| p.shape.clone())
        .expect("primitive");
    let file = regia_scene::prelude::snapshot_scene(world, &Default::default()).expect("snapshot");
    let text = file.to_ron_string().expect("ron");
    assert!(
        text.contains("Primitive"),
        "the level stores the component:\n{text}"
    );

    let mut reloaded = edit_world();
    let parsed = regia_scene::prelude::SceneFile::from_ron_str(&text).expect("parse");
    regia_scene::prelude::spawn_scene(reloaded.world_mut(), &parsed, &Default::default()).expect("spawn");
    let world = reloaded.world_mut();
    let shapes: Vec<PrimitiveShape> = world
        .query::<&Primitive>()
        .iter(world)
        .map(|p| p.shape.clone())
        .collect();
    assert_eq!(shapes, vec![shape]);
}

/// The Outliner eye (ADR-7204): a red shape hidden with `viewport_ops::set_visible` leaves the
/// viewport picture (its red pixels go), and one undo shows it again. No golden: the hidden frame
/// is compared with the shown one.
#[test]
fn the_outliner_eye_hides_a_shape_in_the_viewport_and_undo_shows_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let mut app = edit_world();
    let world = app.world_mut();
    world.insert_resource(regia_editor::command::UndoStack::new());
    world.insert_resource(Selection::default());
    let entity = create_shape(world, "cuboid").expect("created");
    if let Some(mut transform) = world.get_mut::<Transform>(entity) {
        transform.translation = Vec3::new(0.0, 0.6, 0.0);
    }
    if let Some(mut primitive) = world.get_mut::<Primitive>(entity) {
        primitive.color = [0.9, 0.05, 0.05, 1.0];
    }
    let camera = EditorCamera {
        target: Vec3::new(0.0, 0.5, 0.0),
        distance: 5.0,
        ..EditorCamera::default()
    };
    let red = |image: &egui::ColorImage| {
        image
            .pixels
            .iter()
            .filter(|p| u32::from(p.r()) > u32::from(p.g()) + 60 && u32::from(p.r()) > u32::from(p.b()) + 60)
            .count()
    };
    let shown = render_viewport(&device, world, &camera, [320.0, 240.0]).expect("render shown");
    assert!(
        red(&shown) > 500,
        "the red cube draws ({} red pixels)",
        red(&shown)
    );

    assert!(regia_editor::viewport_ops::set_visible(world, entity, false));
    let hidden = render_viewport(&device, world, &camera, [320.0, 240.0]).expect("render hidden");
    assert!(
        red(&hidden) * 50 < red(&shown),
        "the hidden cube is not drawn ({} red pixels hidden, {} shown)",
        red(&hidden),
        red(&shown)
    );

    assert!(
        regia_editor::command::undo_world(world),
        "the eye is an undo step"
    );
    let back = render_viewport(&device, world, &camera, [320.0, 240.0]).expect("render back");
    assert!(
        red(&back) > 500,
        "undo shows it again ({} red pixels)",
        red(&back)
    );
}
