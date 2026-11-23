// SPDX-License-Identifier: MPL-2.0
//! The viewport renders the real frame through the runtime path (`docs/specs/19-editor.md`
//! § 3): proxies for empty entities, real meshes and materials from `MeshSource`, lights,
//! the selection outline, GPU picking and box select, orthographic panes and quality presets.

use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_editor::render::{PaneRequest, capture_pane, capture_viewport, render_pane, render_viewport};
use regia_editor::selection::{SelectMode, Selection};
use regia_editor::viewport::{self, EditorCamera};
use regia_editor::views::{ViewKind, ViewportLayout, ViewportQuality};
use regia_render::{MeshSource, PointLight, RenderProfilePreset};
use regia_scene::prelude::Transform;

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

fn mean_abs_delta(a: &egui::ColorImage, b: &egui::ColorImage) -> f64 {
    assert_eq!(a.pixels.len(), b.pixels.len());
    let mut sum = 0u64;
    for (l, r) in a.pixels.iter().zip(&b.pixels) {
        sum += u64::from(l.r().abs_diff(r.r()))
            + u64::from(l.g().abs_diff(r.g()))
            + u64::from(l.b().abs_diff(r.b()));
    }
    sum as f64 / (a.pixels.len().max(1) as f64 * 3.0)
}

fn request(kind: ViewKind, camera: EditorCamera) -> PaneRequest {
    PaneRequest {
        pane: 0,
        kind,
        camera,
        width: 320,
        height: 240,
        ids: true,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    }
}

fn shot_dir() -> std::path::PathBuf {
    let path = regia_core::brand::env_var("REGIA_VIEWPORT_SHOTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-viewport-shots"));
    let _ = std::fs::create_dir_all(&path);
    path
}

fn save(image: &egui::ColorImage, name: &str) {
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
        .collect();
    if let Some(buffer) = image::RgbaImage::from_raw(image.width() as u32, image.height() as u32, bytes) {
        let path = shot_dir().join(name);
        let _ = buffer.save(&path);
        eprintln!("shot {}", path.display());
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn viewport_renders_lit_proxies_with_selection_outline() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = World::new();
    let near = world
        .spawn(Transform {
            translation: Vec3::new(2.5, 0.6, -1.5),
            ..Default::default()
        })
        .id();
    let _far = world
        .spawn(Transform {
            translation: Vec3::new(-2.0, 0.5, 1.0),
            scale: Vec3::new(1.5, 1.5, 1.5),
            ..Default::default()
        })
        .id();
    let mut selection = Selection::default();
    selection.select(near, SelectMode::Replace);
    world.insert_resource(selection);
    // Editor-internal entity: hidden, must not add a proxy.
    let hidden = world.spawn(Transform::default()).id();
    world
        .get_entity_mut(hidden)
        .expect("alive")
        .insert(regia_editor::visibility::EditorHidden);

    let camera = EditorCamera::default();
    let image =
        render_viewport(&device, &mut world, &camera, [320.0, 240.0]).expect("the viewport render succeeds");
    assert_eq!((image.width(), image.height()), (320, 240));

    // The old failure mode was a black viewport. The report measures the real content: a
    // visible sky and lit proxies with scan-line shading variation.
    let raw_report = viewport::measure_viewport(&image);
    assert!(raw_report.sky_visible(), "the viewport has a sky: {raw_report:?}");
    assert!(raw_report.shading_visible(), "lit proxies shade: {raw_report:?}");
    // The picture itself carries no overlay: the selection is an outline added on top.
    assert!(!raw_report.selection_highlight, "{raw_report:?}");

    // The protocol capture adds the outline, the grid and the gizmo. Sky, shading, outline,
    // grid and gizmo all have to be present — a near-black clear fails this.
    let captured = capture_viewport(&device, &mut world, &camera, [320.0, 240.0]).expect("capture");
    let report = viewport::measure_viewport(&captured);
    assert!(
        report.failures().is_empty(),
        "viewport measurements: {report:?} missing {:?}",
        report.failures()
    );
    save(&captured, "proxies_outline.png");
}

#[test]
fn hidden_entities_render_no_proxy() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut empty = World::new();
    let mut world = World::new();
    let hidden = world.spawn(Transform::default()).id();
    world
        .get_entity_mut(hidden)
        .expect("alive")
        .insert(regia_editor::visibility::EditorHidden);
    let camera = EditorCamera::default();
    let with_hidden = render_viewport(&device, &mut world, &camera, [320.0, 240.0]).expect("render");
    let without = render_viewport(&device, &mut empty, &camera, [320.0, 240.0]).expect("render");
    let delta = mean_abs_delta(&with_hidden, &without);
    assert!(
        delta < 0.01,
        "a hidden entity adds nothing to the picture: {delta}"
    );
}

/// A world with a real mesh (built-in sphere, red `.material.ron`), a point light and an
/// empty entity (proxy).
fn mesh_world(dir: &std::path::Path) -> (World, Entity, Entity) {
    std::fs::create_dir_all(dir.join("assets/materials")).expect("dir");
    let mut red = regia_asset::MaterialAssetRon::default_standard();
    red.base.base_color = [0.85, 0.08, 0.05, 1.0];
    red.base.roughness = 0.4;
    std::fs::write(
        dir.join("assets/materials/red.material.ron"),
        red.to_ron_string().expect("ron"),
    )
    .expect("write material");
    let mut world = World::new();
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    let sphere = world
        .spawn((
            Transform {
                translation: Vec3::new(0.0, 1.0, 0.0),
                scale: Vec3::splat(1.6),
                ..Default::default()
            },
            MeshSource::new(MeshSource::BUILTIN_SPHERE, "assets/materials/red.material.ron"),
        ))
        .id();
    world.spawn((
        Transform {
            translation: Vec3::new(0.0, 0.0, 0.0),
            scale: Vec3::new(8.0, 1.0, 8.0),
            ..Default::default()
        },
        MeshSource::new(MeshSource::BUILTIN_PLANE, ""),
    ));
    world.spawn((
        Transform::from_translation(Vec3::new(1.5, 2.5, 1.5)),
        PointLight {
            intensity: 4000.0,
            radius: 8.0,
            ..PointLight::default()
        },
    ));
    let empty = world
        .spawn(Transform {
            translation: Vec3::new(-2.0, 0.5, -1.0),
            scale: Vec3::splat(0.6),
            ..Default::default()
        })
        .id();
    (world, sphere, empty)
}

#[test]
fn real_meshes_render_pick_and_box_select() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let dir = std::env::temp_dir().join(format!("regia-viewport-meshes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (mut world, sphere, empty) = mesh_world(&dir);
    let camera = EditorCamera {
        target: Vec3::new(0.0, 0.8, 0.0),
        distance: 7.0,
        ..EditorCamera::default()
    };
    let frame = render_pane(
        &device,
        &mut world,
        &request(ViewKind::Perspective, camera.clone()),
    )
    .expect("render");
    assert_eq!(frame.info.meshes, 2, "sphere + plane extracted: {:?}", frame.info);
    assert_eq!(frame.info.proxies, 1, "only the empty entity is a proxy");
    assert_eq!(frame.info.local_lights, 1);
    assert!(
        frame.info.preview_sun,
        "no sun in the level: the preview sun stands in"
    );

    save(&frame.image, "meshes_perspective_raw.png");
    // The sphere is the red material, not a grey proxy.
    let size = Vec2::new(320.0, 240.0);
    let center = frame
        .view
        .project(Vec3::new(0.0, 1.0, 0.0), size)
        .expect("on screen");
    let px = frame.image.pixels[center.y as usize * 320 + center.x as usize];
    assert!(
        px.r() > px.g().saturating_add(60) && px.r() > px.b().saturating_add(60),
        "red sphere: {px:?}"
    );

    // GPU picking: the sphere's centre, the proxy, and the background.
    let picked = viewport::pick_in_frame(&world, &frame, center).map(|h| h.0);
    assert_eq!(picked, Some(sphere));
    let proxy_px = frame
        .view
        .project(Vec3::new(-2.0, 0.5, -1.0), size)
        .expect("on screen");
    assert_eq!(
        viewport::pick_in_frame(&world, &frame, proxy_px).map(|h| h.0),
        Some(empty)
    );
    assert_eq!(
        viewport::pick_in_frame(&world, &frame, Vec2::new(4.0, 4.0)),
        None,
        "sky"
    );

    // Pixel precision: walk right from the centre to the silhouette; the last sphere pixel
    // and the first pixel after it are 1 px apart and pick differently.
    let ids = frame.ids.as_ref().expect("ids");
    let row = center.y as u32;
    let mut x = center.x as u32;
    while ids.at(x + 1, row).map(|h| h.0) == Some(sphere) {
        x += 1;
    }
    assert_eq!(ids.at(x, row).map(|h| h.0), Some(sphere));
    assert_ne!(ids.at(x + 1, row).map(|h| h.0), Some(sphere));

    // Box select over the whole frame finds the sphere, the plane and the proxy.
    let all = ids.entities_in_rect((0, 0), (319, 239));
    assert!(
        all.contains(&sphere) && all.contains(&empty) && all.len() == 3,
        "{all:?}"
    );

    // Selection outline + bounds in the capture.
    let mut selection = Selection::default();
    selection.select(sphere, SelectMode::Replace);
    world.insert_resource(selection);
    let captured =
        capture_pane(&device, &mut world, &request(ViewKind::Perspective, camera)).expect("capture");
    let report = viewport::measure_viewport(&captured.image);
    assert!(report.selection_highlight, "outline: {report:?}");
    assert!(
        captured.image.pixels.contains(&regia_editor::overlays::BOUNDS),
        "bounds drawn"
    );
    assert!(
        captured
            .image
            .pixels
            .contains(&regia_editor::overlays::ICON_LIGHT),
        "light icon drawn"
    );
    save(&captured.image, "meshes_perspective.png");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn orthographic_panes_render_and_pick() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let dir = std::env::temp_dir().join(format!("regia-viewport-ortho-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (mut world, sphere, _) = mesh_world(&dir);
    let camera = EditorCamera {
        target: Vec3::new(0.0, 1.0, 0.0),
        distance: 6.0,
        ..EditorCamera::default()
    };
    for kind in [ViewKind::Top, ViewKind::Front, ViewKind::Side] {
        let frame = capture_pane(&device, &mut world, &request(kind, camera.clone())).expect("render");
        save(&frame.image, &format!("meshes_{}.png", kind.name()));
        assert!(frame.view.ortho);
        let center = frame
            .view
            .project(Vec3::new(0.0, 1.0, 0.0), Vec2::new(320.0, 240.0))
            .expect("on screen");
        assert!(
            (center.x - 160.0).abs() < 1.0 && (center.y - 120.0).abs() < 1.0,
            "{kind:?} {center:?}"
        );
        let hit = frame.ids.as_ref().and_then(|ids| ids.at(160, 120)).map(|h| h.0);
        assert_eq!(hit, Some(sphere), "{kind:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn game_quality_renders_the_atmosphere_and_game_view_hides_helpers() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let dir = std::env::temp_dir().join(format!("regia-viewport-game-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (mut world, sphere, _) = mesh_world(&dir);
    let mut selection = Selection::default();
    selection.select(sphere, SelectMode::Replace);
    world.insert_resource(selection);
    world.insert_resource(ViewportLayout {
        quality: ViewportQuality::Profile(RenderProfilePreset::DesktopHigh),
        game_view: true,
        ..ViewportLayout::default()
    });
    let camera = EditorCamera {
        target: Vec3::new(0.0, 1.0, 0.0),
        distance: 7.0,
        ..EditorCamera::default()
    };
    let mut req = request(ViewKind::Perspective, camera);
    req.warm_frames = 3;
    let frame = capture_pane(&device, &mut world, &req).expect("render");
    assert!(
        frame.info.atmosphere,
        "desktop high draws the physical sky: {:?}",
        frame.info
    );
    assert_eq!(frame.info.quality, "desktop_high");
    assert_eq!(frame.info.proxies, 0, "game view draws no proxies");
    let report = viewport::measure_viewport(&frame.image);
    assert_eq!(report.grid_pixels, 0, "game view: no grid");
    assert!(
        !report.selection_highlight && report.gizmo_axes == 0,
        "game view: no selection helpers"
    );
    assert!(report.lit_peak > 0.2, "the frame is lit: {report:?}");
    save(&frame.image, "meshes_desktop_high_game_view.png");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A 129² synthetic mountain terrain entity; returns the world, the entity and the ground
/// height at the origin.
fn mountain_world() -> (World, Entity, f32) {
    use regia_terrain::{CdlodConfig, MinMaxPyramid, Terrain, TerrainLayers, TerrainMap};
    let n = 129;
    let heights = regia_terrain::synthetic_mountains(n, 256.0, 30.0, 7);
    let map = TerrainMap::from_heights(n, n, heights, 256.0, 1.0, 0.0).expect("map");
    let root_level = 4;
    let minmax = MinMaxPyramid::build(&map, root_level).expect("pyramid");
    let ground = map.sample_bilinear(0.0, 0.0);
    let mut world = World::new();
    let terrain = world
        .spawn((
            Transform::default(),
            Terrain {
                map,
                cdlod: CdlodConfig::default(),
                root_level,
                minmax,
                layers: TerrainLayers::default(),
                holes: None,
                water: None,
            },
        ))
        .id();
    (world, terrain, ground)
}

#[test]
fn terrain_renders_through_the_shared_draw_and_picks() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (mut world, terrain, ground) = mountain_world();
    let camera = EditorCamera {
        target: Vec3::new(0.0, ground, 0.0),
        distance: 60.0,
        pitch: 0.5,
        ..EditorCamera::default()
    };
    let frame = capture_pane(&device, &mut world, &request(ViewKind::Perspective, camera)).expect("render");
    save(&frame.image, "terrain_perspective.png");
    let (hit, position) =
        viewport::pick_in_frame(&world, &frame, Vec2::new(160.0, 120.0)).expect("terrain hit");
    assert_eq!(hit, terrain, "the center pixel is terrain");
    assert!(
        (position.y - ground).abs() < 3.0,
        "hit near the target height: {position:?} vs {ground}"
    );
    // The centre is drawn ground, not the backdrop sky: not blue-dominant.
    let p = frame.image.pixels[120 * 320 + 160];
    assert!(
        !(p.b() > p.r() + 20 && p.b() > p.g()),
        "terrain at the center, got sky-like {p:?}"
    );
}

#[test]
fn terrain_tool_sculpts_on_the_preview_and_the_game_draw_rebuilds() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use regia_editor::terrain_tool::TerrainTool;
    use regia_terrain::{Brush, BrushMode, BrushShape, Terrain, TerrainEdits, apply_stroke};
    let device = device();
    let (mut world, terrain, ground) = mountain_world();
    let camera = EditorCamera {
        target: Vec3::new(0.0, ground, 0.0),
        distance: 90.0,
        pitch: 0.45,
        ..EditorCamera::default()
    };
    let req = request(ViewKind::Perspective, camera);
    let not_sky = |image: &egui::ColorImage| {
        let p = image.pixels[120 * 320 + 160];
        !(p.b() > p.r() + 20 && p.b() > p.g())
    };
    let game = capture_pane(&device, &mut world, &req).expect("game draw");

    // Tool on: the preview mesh (vertex-coloured layers) stands in for the game's draw.
    let mut tool = TerrainTool::default();
    tool.active = true;
    world.insert_resource(tool);
    let preview = capture_pane(&device, &mut world, &req).expect("preview");
    save(&preview.image, "terrain_sculpt_preview.png");
    assert!(
        not_sky(&preview.image),
        "the preview draws the ground at the center"
    );
    let swap = mean_abs_delta(&game.image, &preview.image);
    assert!(
        swap > 0.5,
        "the sculpt preview replaces the game draw (mean delta {swap:.2})"
    );

    // A 25 m raise at the centre shows on the preview at once.
    let brush = Brush {
        shape: BrushShape::Circle { radius: 40.0 },
        mode: BrushMode::Raise { amount: 25.0 },
        hardness: 0.6,
        ..Brush::default()
    };
    let mut edits = TerrainEdits::default();
    {
        let mut entity = world.entity_mut(terrain);
        let mut map = entity.get_mut::<Terrain>().expect("terrain");
        apply_stroke(
            &mut map,
            None,
            &mut edits,
            &brush,
            &[regia_core::math::Vec2::ZERO],
        )
        .expect("stroke");
    }
    let sculpted = capture_pane(&device, &mut world, &req).expect("sculpted preview");
    save(&sculpted.image, "terrain_sculpt_raised.png");
    let raised = mean_abs_delta(&preview.image, &sculpted.image);
    assert!(
        raised > 0.3,
        "the stroke shows on the preview (mean delta {raised:.2})"
    );

    // Tool off: the game draw comes back, rebuilt from the edited map.
    world.resource_mut::<TerrainTool>().active = false;
    let after = capture_pane(&device, &mut world, &req).expect("game draw after");
    save(&after.image, "terrain_sculpt_game_after.png");
    assert!(not_sky(&after.image), "the game draw is back at the center");
    let rebuilt = mean_abs_delta(&game.image, &after.image);
    assert!(
        rebuilt > 0.3,
        "the game draw shows the raised ground (mean delta {rebuilt:.2})"
    );
    let back = mean_abs_delta(&preview.image, &after.image);
    assert!(back > 0.5, "the preview is gone (mean delta {back:.2})");
}

#[test]
fn vfx_draw_through_the_world_runtime_renderer() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use regia_app::prelude::*;
    let device = device();
    let build = |with_smoke: bool| {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(50),
        ));
        app.insert_resource(device.clone());
        app.init_resource::<regia_render::RuntimeRenderer>();
        let mut lib = regia_vfx::scene::VfxLibrary::new();
        lib.add(regia_vfx::showcase::smoke()).expect("smoke");
        app.insert_resource(regia_vfx::VfxScene::new(lib));
        app.add_plugins((regia_vfx::VfxPlugin, regia_vfx::VfxRuntimePlugin));
        if with_smoke {
            app.world_mut()
                .resource_mut::<regia_vfx::VfxScene>()
                .spawn("smoke", Vec3::ZERO, Vec3::Z, 7)
                .expect("spawn smoke");
        }
        app
    };
    let camera = EditorCamera {
        target: Vec3::new(0.0, 3.0, 0.0),
        distance: 14.0,
        pitch: 0.2,
        ..EditorCamera::default()
    };
    let mut shots = Vec::new();
    for with_smoke in [false, true] {
        let mut app = build(with_smoke);
        let mut last = None;
        for _ in 0..30 {
            app.update();
            last = render_pane(
                &device,
                app.world_mut(),
                &request(ViewKind::Perspective, camera.clone()),
            );
        }
        let frame = last.expect("render");
        assert!(
            frame.info.runtime_renderer,
            "the world's RuntimeRenderer drew the pane"
        );
        shots.push(frame.image);
    }
    save(&shots[1], "vfx_smoke.png");
    let delta = mean_abs_delta(&shots[0], &shots[1]);
    assert!(
        delta > 0.5,
        "the smoke shows in the editor viewport: mean delta {delta:.3}"
    );
}

/// ADR-7768 (File ▸ Screenshot…): a 2x screenshot is the 1x picture at twice the size. Its size is
/// exact, both fit one tile under the device limit (the same tile-count math), and on a flat scene
/// (an empty level: the backdrop sky only, helpers off) each 2x2 block of the 2x picture averages to
/// the 1x pixel it covers. The file lands in the project's Saved/Screenshots folder.
#[test]
fn a_2x_screenshot_is_the_1x_picture_upsampled_and_exactly_sized() {
    use regia_editor::screenshot::{self, ShotSize};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = World::new();
    world.insert_resource(EditorCamera::default());
    let pane = [96, 64];
    let one_size = screenshot::resolve_size(ShotSize::Multiplier(1), pane).expect("1x");
    let two_size = screenshot::resolve_size(ShotSize::Multiplier(2), pane).expect("2x");
    assert_eq!(two_size, [192, 128]);
    let limit = screenshot::device_limit(&device);
    assert_eq!(
        screenshot::plan_tiles(one_size, limit).count(),
        screenshot::plan_tiles(two_size, limit).count()
    );
    let one = screenshot::render(&device, &mut world, one_size, false).expect("1x render");
    let two = screenshot::render(&device, &mut world, two_size, false).expect("2x render");
    assert_eq!(one.size, [96, 64]);
    assert_eq!(two.size, [192, 128], "exact size");
    let mut total = 0.0f64;
    let mut worst = 0.0f64;
    for y in 0..64usize {
        for x in 0..96usize {
            let p = one.pixels[y * 96 + x];
            let block = [
                two.pixels[(2 * y) * 192 + 2 * x],
                two.pixels[(2 * y) * 192 + 2 * x + 1],
                two.pixels[(2 * y + 1) * 192 + 2 * x],
                two.pixels[(2 * y + 1) * 192 + 2 * x + 1],
            ];
            for channel in 0..3 {
                let get = |c: egui::Color32| f64::from([c.r(), c.g(), c.b()][channel]);
                let mean = block.iter().map(|c| get(*c)).sum::<f64>() / 4.0;
                let delta = (mean - get(p)).abs();
                total += delta;
                worst = worst.max(delta);
            }
        }
    }
    let mean = total / (96.0 * 64.0 * 3.0);
    save(&one, "screenshot_1x");
    save(&two, "screenshot_2x");
    assert!(
        mean < 2.0,
        "2x blocks average to the 1x pixels: mean {mean:.3}, worst {worst:.1}"
    );

    let root = regia_test_utils::unique_temp_dir("viewport-screenshot");
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    let shot = screenshot::take(&device, &mut world, two_size, true, None).expect("take");
    assert_eq!(shot.path, root.join("Saved/Screenshots/screenshot-0001.png"));
    let decoder = png::Decoder::new(std::io::BufReader::new(
        std::fs::File::open(&shot.path).expect("file"),
    ));
    let reader = decoder.read_info().expect("png");
    assert_eq!((reader.info().width, reader.info().height), (192, 128));
    let _ = std::fs::remove_dir_all(&root);
}

/// Calls an editor agent method in process (the dispatcher's handler for `method`).
fn agent_call(world: &mut World, method: &str, params: serde_json::Value) -> serde_json::Value {
    if world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .is_none()
    {
        regia_editor::agent_ext::install(world);
    }
    let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).unwrap_or_else(|err| panic!("{method}: {err:?}"))
}

/// P-064 acceptance (ADR-7769) on the real id buffer: three cubes in a row; a lasso drawn around
/// the two on the left selects exactly those, a Ctrl+Shift (subtract) lasso around one removes
/// it, and a subtract box over the other empties the selection.
#[test]
fn a_lasso_over_two_of_three_cubes_selects_those_and_subtract_removes_one() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = World::new();
    world.insert_resource(device);
    world.insert_resource(Selection::default());
    let camera = EditorCamera {
        target: Vec3::ZERO,
        distance: 9.0,
        yaw: 0.0,
        pitch: 0.0,
        ..EditorCamera::default()
    };
    world.insert_resource(camera.clone());
    let cube = |world: &mut World, x: f32| {
        let (vertices, indices) = regia_render::box_mesh([0.4, 0.4, 0.4]);
        let transform = Transform::from_translation(Vec3::new(x, 0.0, 0.0));
        world
            .spawn((
                transform,
                regia_scene::GlobalTransform(transform.compute_affine()),
                regia_render::MeshRenderer::new(
                    std::sync::Arc::new(regia_render::CpuMesh { vertices, indices }),
                    std::sync::Arc::new(regia_render::CpuMaterial::default()),
                ),
            ))
            .id()
    };
    let (a, b, c) = (
        cube(&mut world, -2.0),
        cube(&mut world, 0.0),
        cube(&mut world, 2.0),
    );
    let view = regia_editor::views::ViewCamera::of(ViewKind::Perspective, &camera);
    let size = Vec2::new(320.0, 240.0);
    let px = |p: Vec3| view.project(p, size).expect("on screen");
    let (pa, pb, pc) = (
        px(Vec3::new(-2.0, 0.0, 0.0)),
        px(Vec3::ZERO),
        px(Vec3::new(2.0, 0.0, 0.0)),
    );
    assert!(pa.x < pb.x && pb.x < pc.x, "{pa} {pb} {pc}");
    // A diamond around a and b that stops halfway to c.
    let right = (pb.x + pc.x) * 0.5;
    let lasso = serde_json::json!([
        [pa.x - 30.0, pa.y],
        [(pa.x + pb.x) * 0.5, pa.y - 40.0],
        [right, pb.y],
        [(pa.x + pb.x) * 0.5, pa.y + 40.0],
    ]);
    let bits = |world: &World| -> Vec<u64> {
        let mut out: Vec<u64> = world
            .resource::<Selection>()
            .entities()
            .iter()
            .map(|e| e.to_bits())
            .collect();
        out.sort_unstable();
        out
    };
    let mut ab = vec![a.to_bits(), b.to_bits()];
    ab.sort_unstable();
    agent_call(
        &mut world,
        "editor.lasso_select",
        serde_json::json!({ "points": lasso, "width": 320, "height": 240 }),
    );
    assert_eq!(bits(&world), ab, "the lasso takes a and b, not c");
    let around_b = serde_json::json!([
        [pb.x - 15.0, pb.y - 15.0],
        [pb.x + 15.0, pb.y - 15.0],
        [pb.x + 15.0, pb.y + 15.0],
        [pb.x - 15.0, pb.y + 15.0],
    ]);
    agent_call(
        &mut world,
        "editor.lasso_select",
        serde_json::json!({ "points": around_b, "mode": "subtract", "width": 320, "height": 240 }),
    );
    assert_eq!(bits(&world), vec![a.to_bits()], "subtract removed b");
    agent_call(
        &mut world,
        "editor.box_select",
        serde_json::json!({
            "x0": pa.x - 10.0, "y0": pa.y - 10.0, "x1": pa.x + 10.0, "y1": pa.y + 10.0,
            "mode": "subtract", "width": 320, "height": 240,
        }),
    );
    assert!(bits(&world).is_empty(), "a subtract box removed a");
    assert!(!world.resource::<Selection>().contains(c));
    // The pick stack on the id buffer: a ray through b's center lists b first.
    let stack = agent_call(
        &mut world,
        "editor.pick_stack",
        serde_json::json!({ "x": pb.x, "y": pb.y, "width": 320, "height": 240 }),
    );
    assert_eq!(stack["method"], "gpu", "{stack}");
    assert_eq!(
        stack["entities"][0]["entity"].as_u64(),
        Some(b.to_bits()),
        "{stack}"
    );
}
