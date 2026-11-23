// SPDX-License-Identifier: MPL-2.0
//! Surface debug views (Tincal engine request F3): UV checker, texel density and material ID
//! change the picture deterministically, texel density classifies a known UV scale, and the
//! material ID legend names every material with its colour. `Lit` stays untouched afterwards.

use std::sync::Arc;

use regia_core::math::Vec3;
use regia_editor::render::{PaneFrame, PaneRequest, render_pane};
use regia_editor::viewport::EditorCamera;
use regia_editor::views::ViewKind;
use regia_render::{
    CpuMesh, DebugViewMode, LabMaterial, LabVertex, MaterialLabel, MeshRenderer, box_mesh, material_color,
};
use regia_scene::prelude::Transform;

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

/// A 1.4 m square on the ground (XZ, facing up) whose UVs span `0..uv`.
fn ground_quad(uv: f32) -> CpuMesh {
    let v = |x: f32, z: f32, u: f32, w: f32| {
        LabVertex::from_pos_normal_uv(
            [x * 1.4 - 0.7, 0.0, z * 1.4 - 0.7],
            [0.0, 1.0, 0.0],
            [u * uv, w * uv],
        )
    };
    CpuMesh {
        vertices: vec![
            v(0.0, 0.0, 0.0, 0.0),
            v(1.0, 0.0, 1.0, 0.0),
            v(1.0, 1.0, 1.0, 1.0),
            v(0.0, 1.0, 0.0, 1.0),
        ],
        indices: vec![0, 2, 1, 0, 3, 2],
    }
}

fn base_app() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app.world_mut().insert_resource(EditorCamera::default());
    app
}

fn grey() -> Arc<LabMaterial> {
    Arc::new(LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        double_sided: true,
        ..LabMaterial::default()
    })
}

/// Three quads of one metre-scale with UV extents 0.25, 0.5 and 1.0. With no base-colour
/// texture the density assumes 1024 px, so the quads are 1024 * uv / 1.4 = 183, 366 and
/// 731 px/m.
fn density_scene() -> regia_app::App {
    let mut app = base_app();
    let material = grey();
    for (i, uv) in [0.25_f32, 0.5, 1.0].into_iter().enumerate() {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(-1.7 + 1.7 * i as f32, 0.0, 0.0)),
            MeshRenderer::new(Arc::new(ground_quad(uv)), Arc::clone(&material)),
        ));
    }
    app
}

fn request(debug_view: DebugViewMode) -> PaneRequest {
    PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera {
            target: Vec3::ZERO,
            distance: 6.0,
            yaw: 0.0,
            pitch: 1.2,
            ..EditorCamera::default()
        },
        width: 480,
        height: 320,
        ids: false,
        warm_frames: 0,
        debug_view,
        reference: None,
    }
}

fn shoot(app: &mut regia_app::App, mode: DebugViewMode, name: &str) -> PaneFrame {
    let frame = render_pane(&device(), app.world_mut(), &request(mode)).expect("render");
    let dir = regia_core::brand::env_var("REGIA_VIEWPORT_SHOTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-viewport-shots"));
    let _ = std::fs::create_dir_all(&dir);
    let bytes: Vec<u8> = frame
        .image
        .pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
        .collect();
    if let Some(buffer) =
        image::RgbaImage::from_raw(frame.image.width() as u32, frame.image.height() as u32, bytes)
    {
        let _ = buffer.save(dir.join(format!("debug_surface_{name}.png")));
    }
    frame
}

fn count(frame: &PaneFrame, f: impl Fn(&egui::Color32) -> bool) -> usize {
    frame.image.pixels.iter().filter(|p| f(p)).count()
}

fn blue(p: &egui::Color32) -> bool {
    p.b() > 200 && p.r() < 120 && p.g() < 170
}
fn green(p: &egui::Color32) -> bool {
    p.g() > 180 && p.r() < 120 && p.b() < 130
}
fn red(p: &egui::Color32) -> bool {
    p.r() > 200 && p.g() < 120 && p.b() < 110
}

fn density(target: f32, tolerance: f32) -> DebugViewMode {
    DebugViewMode::TexelDensity {
        target_px_per_m: target,
        tolerance,
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn texel_density_classifies_a_known_uv_scale() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = density_scene();
    let lit = shoot(&mut app, DebugViewMode::Lit, "density_lit");
    // 183 / 366 / 731 px/m against a 366 px/m target: under, within, over.
    let frame = shoot(&mut app, density(366.0, 0.1), "density");
    let (b, g, r) = (count(&frame, blue), count(&frame, green), count(&frame, red));
    eprintln!("density 366: blue {b} green {g} red {r}");
    assert!(
        b > 800 && g > 800 && r > 800,
        "all three classes show: {b} {g} {r}"
    );
    assert_eq!(
        count(&lit, blue) + count(&lit, green) + count(&lit, red),
        0,
        "the lit frame has none"
    );

    // Same scene, target 731: the third quad is within, the others both under.
    let high = shoot(&mut app, density(731.0, 0.1), "density_high");
    assert_eq!(count(&high, red), 0, "nothing exceeds the target");
    assert!(
        count(&high, green) > 800 && count(&high, blue) > count(&frame, blue),
        "blue grows"
    );

    // A wide tolerance takes everything in.
    let wide = shoot(&mut app, density(366.0, 2.0), "density_wide");
    assert_eq!(
        count(&wide, blue) + count(&wide, red),
        0,
        "tolerance 2.0 accepts all"
    );

    // Deterministic, and no state left behind.
    let again = shoot(&mut app, density(366.0, 0.1), "density_again");
    assert_eq!(frame.image.pixels, again.image.pixels);
    let lit_again = shoot(&mut app, DebugViewMode::Lit, "density_lit_again");
    assert_eq!(lit.image.pixels, lit_again.image.pixels);
}

#[test]
fn uv_checker_replaces_the_shading_and_follows_tile_size() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = density_scene();
    let lit = shoot(&mut app, DebugViewMode::Lit, "uv_lit");
    let fine = shoot(&mut app, DebugViewMode::UvChecker { tile_m: 0.1 }, "uv_fine");
    let coarse = shoot(&mut app, DebugViewMode::UvChecker { tile_m: 0.4 }, "uv_coarse");
    assert_ne!(lit.image.pixels, fine.image.pixels);
    assert_ne!(
        fine.image.pixels, coarse.image.pixels,
        "tile size changes the pattern"
    );

    // The checker alternates light and dark cells; count brightness transitions along rows.
    let transitions = |frame: &PaneFrame| {
        let w = frame.image.width();
        let luma = |p: &egui::Color32| u32::from(p.r()) + u32::from(p.g()) + u32::from(p.b());
        frame
            .image
            .pixels
            .chunks(w)
            .map(|row| {
                row.windows(2)
                    .filter(|p| luma(&p[0]).abs_diff(luma(&p[1])) > 120)
                    .count()
            })
            .sum::<usize>()
    };
    let (tf, tc, tl) = (transitions(&fine), transitions(&coarse), transitions(&lit));
    eprintln!("uv checker transitions: fine {tf} coarse {tc} lit {tl}");
    assert!(
        tf > tc * 3 / 2,
        "0.1 m cells make more edges than 0.4 m cells ({tf} vs {tc})"
    );
    assert!(
        tc > tl + 20,
        "the checker has edges the lit frame does not ({tc} vs {tl})"
    );

    let again = shoot(
        &mut app,
        DebugViewMode::UvChecker { tile_m: 0.1 },
        "uv_fine_again",
    );
    assert_eq!(fine.image.pixels, again.image.pixels, "deterministic");
}

#[test]
fn material_id_draws_one_colour_per_material_and_fills_the_legend() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = base_app();
    let world = app.world_mut();
    let (vertices, indices) = box_mesh([0.5, 0.5, 0.5]);
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let skin = grey();
    let metal = Arc::new(LabMaterial {
        base_color: [0.9, 0.9, 0.95, 1.0],
        metallic: 1.0,
        ..LabMaterial::default()
    });
    for (x, material, name) in [
        (-1.5, &skin, "skin"),
        (0.0, &metal, "metal"),
        (1.5, &skin, "skin"),
    ] {
        world.spawn((
            Transform::from_translation(Vec3::new(x, 0.5, 0.0)),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(material)),
            MaterialLabel(name.to_string()),
        ));
    }
    // An unlabeled mesh gets the base-colour fallback name.
    world.spawn((
        Transform::from_translation(Vec3::new(0.0, 0.5, -1.8)),
        MeshRenderer::new(Arc::clone(&mesh), grey()),
    ));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let seen = Arc::clone(&seen);
        world.insert_resource(regia_render::DebugLegendSink(Arc::new(move |legend| {
            *seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = legend.to_vec();
        })));
    }
    let lit = shoot(&mut app, DebugViewMode::Lit, "matid_lit");
    let frame = shoot(&mut app, DebugViewMode::MaterialId, "matid");
    assert_ne!(lit.image.pixels, frame.image.pixels);

    let legend = &frame.info.debug_legend;
    let names: Vec<&str> = legend.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["metal", "skin", "unnamed #999999"],
        "sorted, de-duplicated by name"
    );
    assert_eq!(legend[0].color, material_color("metal"));
    assert_eq!(legend[1].color, material_color("skin"));
    assert_eq!(
        *seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        *legend,
        "the sink got the same legend"
    );
    assert!(lit.info.debug_legend.is_empty());

    // Each legend colour appears on screen (display-referred, so exact up to rounding).
    for entry in legend {
        let want = entry.color.map(|c| (c * 255.0 + 0.5) as i32);
        let hits = count(&frame, |p| {
            [p.r(), p.g(), p.b()]
                .iter()
                .zip(&want)
                .all(|(&a, &b)| (i32::from(a) - b).abs() <= 10)
        });
        eprintln!("{} {:?}: {hits} px", entry.name, want);
        assert!(hits > 300, "`{}` colour {want:?} shows ({hits} px)", entry.name);
    }
    let again = shoot(&mut app, DebugViewMode::MaterialId, "matid_again");
    assert_eq!(frame.image.pixels, again.image.pixels, "stable colours");
}
