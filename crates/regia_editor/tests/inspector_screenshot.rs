// SPDX-License-Identifier: MPL-2.0
//! Offscreen screenshot of the ported reflection inspector on a component with a `Vec<struct>`
//! and an `Option<enum>` (plus a map, a set, a colour and an asset field), rendered headless with
//! `egui-wgpu` (`regia_editor::ui_capture`). Lavapipe on CI; the PNG goes to
//! `target/evidence/inspector/` for review.

use std::collections::{BTreeMap, HashSet};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::attrs::{AssetFilter, Range, Step, Units};
use regia_editor::command::UndoStack;
use regia_editor::dnd::ContentKind;
use regia_editor::inspector;
use regia_editor::ui_capture::{png_bytes, render_ui_offscreen};
use regia_scene::prelude::{Name, Transform};

/// One stop of a patrol route.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
struct Waypoint {
    position: Vec3,
    #[reflect(@Range(0.0..=20.0), @Step(0.1), @Units("m/s"))]
    speed: f32,
    wait: f32,
}

/// What the squad does.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
enum Mode {
    #[default]
    Idle,
    Patrol {
        radius: f32,
        loops: u32,
    },
    Chase(f32),
}

/// The inspected component.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Squad {
    waypoints: Vec<Waypoint>,
    mode: Option<Mode>,
    stats: BTreeMap<String, f32>,
    tags: HashSet<String>,
    base_color: [f32; 3],
    #[reflect(@AssetFilter("*.ogg"))]
    voice: String,
}

#[test]
fn inspector_screenshot_shows_vec_of_struct_and_option_enum() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<Squad>();
    app.init_resource::<UndoStack>();
    let entity = app
        .world_mut()
        .spawn((
            Name::new("Squad"),
            Transform::default(),
            Squad {
                waypoints: vec![
                    Waypoint {
                        position: Vec3::new(1.0, 0.0, -2.0),
                        speed: 2.0,
                        wait: 0.5,
                    },
                    Waypoint {
                        position: Vec3::new(6.0, 0.0, 3.0),
                        speed: 4.0,
                        wait: 1.0,
                    },
                ],
                mode: Some(Mode::Patrol {
                    radius: 12.0,
                    loops: 3,
                }),
                stats: BTreeMap::from([("armor".to_string(), 25.0), ("hp".to_string(), 100.0)]),
                tags: HashSet::from(["elite".to_string(), "night".to_string()]),
                base_color: [0.8, 0.3, 0.1],
                voice: "assets/sfx/bark.ogg".into(),
            },
        ))
        .id();
    let assets = vec![("assets/sfx/bark.ogg".to_string(), ContentKind::Other)];

    let mut changes = Vec::new();
    let world = app.world_mut();
    let image = render_ui_offscreen(&device, [560, 900], 1.25, 3, |ui| {
        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            ui.strong("Inspector");
            ui.label("Entity: Squad");
            ui.separator();
            egui::CollapsingHeader::new("Squad")
                .default_open(true)
                .show(ui, |ui| {
                    changes.extend(inspector::component_ui(ui, world, entity, "Squad", &assets));
                });
        });
    })
    .expect("the inspector renders offscreen");
    assert!(changes.is_empty(), "drawing alone edits nothing: {changes:?}");
    assert_eq!(app.world().resource::<UndoStack>().undo_depth(), 0);

    let out = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/evidence/inspector/inspector_vec_struct_option_enum.png");
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let png = png_bytes(&image).expect("encode");
    std::fs::write(&out, png).expect("write the screenshot");
    eprintln!("wrote {}", out.display());

    // The panel is not blank: plenty of text/widget pixels over the dark background.
    let bright = image
        .pixels
        .iter()
        .filter(|p| u16::from(p.r()) + u16::from(p.g()) + u16::from(p.b()) > 300)
        .count();
    assert!(
        bright > 2_000,
        "only {bright} bright pixels: the inspector did not draw"
    );
    // The rows reach far down: two waypoints (each a struct grid), the option's variant fields,
    // the map and set rows all add height.
    let lowest_text = (0..image.height())
        .rev()
        .find(|&y| {
            (0..image.width()).any(|x| {
                let p = image.pixels[y * image.width() + x];
                u16::from(p.r()) + u16::from(p.g()) + u16::from(p.b()) > 300
            })
        })
        .unwrap_or(0);
    assert!(
        lowest_text > 500,
        "content ends at row {lowest_text}: containers collapsed?"
    );
    // The colour swatch of `base_color` (linear 0.8/0.3/0.1 → sRGB ≈ 230/147/89).
    let swatch = image
        .pixels
        .iter()
        .filter(|p| p.r().abs_diff(230) < 12 && p.g().abs_diff(147) < 12 && p.b().abs_diff(89) < 12)
        .count();
    assert!(swatch > 40, "no color swatch ({swatch} px)");
}
