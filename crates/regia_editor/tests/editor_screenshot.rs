// SPDX-License-Identifier: MPL-2.0
//! An offscreen screenshot of the whole editor window — egui shell plus the viewport's 3D
//! frame — on a project whose level holds real meshes, materials and a light
//! (`docs/specs/19-editor.md` § 2). The PNG is written for a person to review
//! (`REGIA_EDITOR_SHOTS`, default the temp dir); the test checks the viewport area holds a
//! lit 3D picture rather than the dark clear. A second test captures the window in every built-in
//! editor look (ADR-8585).

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::command::{EditorCommand as _, SpawnEntities, UndoStack};
use regia_editor::selection::{SelectMode, Selection};
use regia_editor::shell::EditorShell;
use regia_render::{MeshSource, PointLight};
use regia_scene::prelude::Transform;

/// X of the test's set piece, clear of the template's marker at the origin.
const SET_X: f32 = 2.5;

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

#[test]
fn editor_window_screenshot_shows_the_real_frame_inside_the_shell() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let parent = std::env::temp_dir().join(format!("regia-editor-shot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).expect("parent");
    regia_editor::project::set_recent_file_for_tests(parent.join("recents.txt"));

    let mut app = edit_app();
    let opened = regia_editor::project::create_and_open(app.world_mut(), "Shot Yard", &parent, "blank")
        .expect("project");
    let root = app
        .world()
        .get_resource::<regia_editor::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .expect("project root");
    let _ = opened;

    // Level content: a red material sphere on a cube pedestal, a ground plane and a light, set
    // beside the blank template's own entities (a ground slab and an empty marker, drawn as
    // proxy cubes) so nothing overlaps.
    std::fs::create_dir_all(root.join("assets/materials")).expect("materials dir");
    let mut red = regia_asset::MaterialAssetRon::default_standard();
    red.base.base_color = [0.8, 0.12, 0.06, 1.0];
    red.base.roughness = 0.35;
    std::fs::write(
        root.join("assets/materials/red.material.ron"),
        red.to_ron_string().expect("ron"),
    )
    .expect("write");
    let sphere = spawn(
        &mut app,
        "Sphere",
        Transform::from_translation(Vec3::new(SET_X, 1.4, 0.0)),
    );
    app.world_mut().entity_mut(sphere).insert(MeshSource::new(
        MeshSource::BUILTIN_SPHERE,
        "assets/materials/red.material.ron",
    ));
    let pedestal = spawn(
        &mut app,
        "Pedestal",
        Transform {
            translation: Vec3::new(SET_X, 0.45, 0.0),
            scale: Vec3::new(1.2, 0.9, 1.2),
            ..Default::default()
        },
    );
    app.world_mut()
        .entity_mut(pedestal)
        .insert(MeshSource::new(MeshSource::BUILTIN_CUBE, ""));
    let floor = spawn(
        &mut app,
        "Floor",
        Transform {
            scale: Vec3::new(14.0, 1.0, 14.0),
            ..Default::default()
        },
    );
    app.world_mut()
        .entity_mut(floor)
        .insert(MeshSource::new(MeshSource::BUILTIN_PLANE, ""));
    let lamp = spawn(
        &mut app,
        "Lamp",
        Transform::from_translation(Vec3::new(SET_X + 2.0, 2.5, 1.5)),
    );
    app.world_mut().entity_mut(lamp).insert(PointLight {
        intensity: 6000.0,
        radius: 10.0,
        color: [1.0, 0.85, 0.6],
        ..PointLight::default()
    });
    let mut selection = Selection::default();
    selection.select(sphere, SelectMode::Replace);
    app.world_mut().insert_resource(selection);
    app.world_mut()
        .insert_resource(regia_editor::viewport::EditorCamera {
            target: Vec3::new(SET_X, 1.0, 0.0),
            distance: 6.5,
            ..Default::default()
        });
    app.update();

    let mut shell = EditorShell::new(Some(device.clone()), Some(root.clone()));
    let ctx = egui::Context::default();
    let mut layout = regia_editor::views::ViewportLayout::default();
    layout.show.stats = true;
    app.world_mut().insert_resource(layout);
    let image = regia_editor::window_capture::capture_editor_window(
        &mut shell,
        &ctx,
        app.world_mut(),
        &device,
        [1280, 800],
        1.0,
        3,
    )
    .expect("window capture");
    assert_eq!((image.width(), image.height()), (1280, 800));

    let shots = regia_core::brand::env_var("REGIA_EDITOR_SHOTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-editor-shots"));
    std::fs::create_dir_all(&shots).expect("shot dir");
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
        .collect();
    let png = image::RgbaImage::from_raw(1280, 800, bytes).expect("size");
    let path = shots.join("editor_window.png");
    png.save(&path).expect("png");
    eprintln!("editor window shot: {}", path.display());

    // The viewport sits in the centre of the default layout. Its middle must hold the lit
    // 3D frame: the red sphere (strongly red pixels) and the sky backdrop (blue-ish pixels),
    // not the 24/24/28 dark clear of a viewport without a picture.
    let (w, h) = (1280usize, 800usize);
    let mut red_px = 0usize;
    let mut sky_px = 0usize;
    for y in h / 5..h / 2 {
        for x in w * 3 / 10..w * 6 / 10 {
            let p = image.pixels[y * w + x];
            if p.r() > 120 && u32::from(p.r()) > u32::from(p.g()) + 60 {
                red_px += 1;
            }
            if p.b() > 150 && p.b() > p.r() {
                sky_px += 1;
            }
        }
    }
    assert!(red_px > 200, "the red sphere is in the viewport: {red_px}");
    assert!(sky_px > 2000, "the sky backdrop fills the viewport: {sky_px}");
    let _ = std::fs::remove_dir_all(&parent);
}

/// Whether two colors match within the rounding of an sRGB render target.
fn near(a: egui::Color32, b: egui::Color32) -> bool {
    a.r().abs_diff(b.r()) <= 2 && a.g().abs_diff(b.g()) <= 2 && a.b().abs_diff(b.b()) <= 2
}

/// ADR-8585: the real editor window in each built-in look (Piercing Graphite, the default; AMOLED
/// Night; Paper; high contrast), chosen through the `appearance.theme` preference as a person would.
/// Each capture is written as `editor_theme_<choice>.png` beside `editor_window.png`; the test
/// checks, without a golden, that the window is painted in the look's panel and text colors and
/// that the looks differ from one another.
#[test]
fn the_editor_window_wears_each_built_in_look() {
    use regia_editor::editor_prefs::{EditorPreferences, PrefValue};
    use regia_editor::theme::{BUILTIN_EDITOR_THEMES, ThemeTokens};
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let parent = std::env::temp_dir().join(format!("regia-editor-theme-shots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).expect("parent");
    regia_editor::project::set_recent_file_for_tests(parent.join("recents.txt"));
    let mut app = edit_app();
    regia_editor::project::create_and_open(app.world_mut(), "Theme Yard", &parent, "blank").expect("project");
    let root = app
        .world()
        .get_resource::<regia_editor::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .expect("project root");
    let shots = regia_core::brand::env_var("REGIA_EDITOR_SHOTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-editor-shots"));
    std::fs::create_dir_all(&shots).expect("shot dir");

    let (w, h) = (1280u32, 800u32);
    let mut shares = Vec::new();
    for (choice, label) in BUILTIN_EDITOR_THEMES {
        app.world_mut()
            .get_resource_or_insert_with(EditorPreferences::default)
            .set("appearance.theme", &PrefValue::Text(choice.to_string()))
            .expect("a built-in theme choice");
        regia_editor::editor_prefs::apply_appearance(app.world_mut());
        let (want, _) = regia_editor::theme::builtin_editor_theme(choice).expect("built in");
        assert_eq!(
            *regia_editor::theme::tokens(app.world()),
            want,
            "{label}: the preference selects the look"
        );
        let mut shell = EditorShell::new(Some(device.clone()), Some(root.clone()));
        let ctx = egui::Context::default();
        let image = regia_editor::window_capture::capture_editor_window(
            &mut shell,
            &ctx,
            app.world_mut(),
            &device,
            [w, h],
            1.0,
            4,
        )
        .expect("window capture");
        let bytes: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
            .collect();
        let path = shots.join(format!("editor_theme_{choice}.png"));
        image::RgbaImage::from_raw(w, h, bytes)
            .expect("size")
            .save(&path)
            .expect("png");
        eprintln!("{label}: {}", path.display());
        let tokens: &ThemeTokens = &want;
        let panel = image.pixels.iter().filter(|p| near(**p, tokens.panel)).count();
        let share = panel as f32 / (w * h) as f32;
        eprintln!("{label}: {:.1} % of the window is the panel color", share * 100.0);
        assert!(
            share > 0.05,
            "{label}: only {:.2} % of the window is the panel color {:?}",
            share * 100.0,
            tokens.panel
        );
        // The theme's text color is drawn somewhere (labels, menus).
        assert!(
            image.pixels.iter().any(|p| near(*p, tokens.text)),
            "{label}: no pixel in the text color {:?}",
            tokens.text
        );
        shares.push((label, tokens.panel));
    }
    for (i, (a, pa)) in shares.iter().enumerate() {
        for (b, pb) in shares.iter().skip(i + 1) {
            assert!(!near(*pa, *pb), "{a} and {b} share a panel color");
        }
    }
    let _ = std::fs::remove_dir_all(&parent);
}
