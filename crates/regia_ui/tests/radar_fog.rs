// SPDX-License-Identifier: MPL-2.0
//! The minimap's fog of war layer (audit gap FW-1): a `RadarFog` on a minimap draws the fog image
//! over the map and under the blips, as a window of its world rect, and the image shows
//! unexplored cells dark, explored cells dim and visible cells clear. No GPU, no golden: the
//! checks read the UI renderer's draw list and the image bytes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::glam::{Affine3A, Vec3};
use regia_scene::GlobalTransform;
use regia_ui::draw::TextureRef;
use regia_ui::game_widgets::*;
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;

const MAP: u64 = 0xABC;
const FOG: u64 = 0xF06;

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, UiPlugin::code_only()));
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(10)));
    app.update();
    app
}

/// 4 x 4 fog texels, row 0 = north: row 0 unexplored, row 1 explored, rows 2 and 3 visible, with
/// one fading cell (64) in row 3.
fn fixture() -> Vec<u8> {
    let mut t = vec![0u8; 16];
    t[4..8].fill(128);
    t[8..16].fill(255);
    t[15] = 64;
    t
}

#[test]
fn minimap_shows_explored_and_visible() {
    let mut app = ui_app();
    let root = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    let rect = [-64.0, -64.0, 64.0, 64.0];
    let minimap = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(0.0),
                    top: Dimension::Points(0.0),
                    ..Edges::default()
                },
                size: [Dimension::Points(256.0), Dimension::Points(256.0)],
                ..Default::default()
            }),
            Radar::minimap(rect).with_map(MAP, [0.0; 4]),
            RadarFog {
                world_rect: rect,
                ..RadarFog::new(FOG)
            },
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(minimap);
    app.world_mut().spawn((
        RadarBlip {
            size_px: 8.0,
            ..RadarBlip::default()
        },
        GlobalTransform(Affine3A::from_translation(Vec3::new(10.0, 0.0, 20.0))),
    ));
    let look = RadarFogLook::default();
    let image = radar_fog_rgba(4, 4, &fixture(), &look);
    {
        let ui = app.world().resource::<SharedUiRenderer>().clone();
        let mut ui = ui.lock();
        ui.queue_image_rgba(MAP, 4, 4, vec![200; 64]);
        ui.queue_dynamic_image(FOG, 4, 4, image.clone().into());
    }
    for _ in 0..3 {
        app.update();
    }

    // The draw list: map, then fog, then the blip, all inside the minimap.
    let quads = app.world().resource::<SharedUiRenderer>().lock().quads().to_vec();
    let index_of = |id: u64| quads.iter().position(|q| q.texture == TextureRef::Image(id));
    let map_at = index_of(MAP).expect("map quad");
    let fog_at = index_of(FOG).expect("fog quad");
    assert!(map_at < fog_at, "the fog draws over the map");
    let blip_at = quads
        .iter()
        .rposition(|q| q.texture == TextureRef::Rect && q.size == [8.0, 8.0])
        .expect("blip quad");
    assert!(fog_at < blip_at, "blips draw over the fog");
    let fog = &quads[fog_at];
    assert_eq!(fog.pos, [0.0, 0.0]);
    assert_eq!(fog.size, [256.0, 256.0]);
    assert_eq!(
        fog.uv,
        [0.0, 0.0, 1.0, 1.0],
        "the whole grid covers the whole minimap"
    );

    // The image: unexplored dark, explored dim, visible clear, a fading cell between.
    let alpha = |col: usize, row: usize| image[(row * 4 + col) * 4 + 3];
    assert_eq!(alpha(0, 0), (0.94f32 * 255.0).round() as u8, "unexplored");
    assert_eq!(alpha(2, 1), 128, "explored");
    assert_eq!(alpha(1, 2), 0, "visible");
    assert!(alpha(3, 3) > 128 && alpha(3, 3) < 240, "fading: {}", alpha(3, 3));
    assert!(image.chunks(4).all(|px| px[..3] == [0, 0, 0]), "black fog");

    // A minimap of another area shows a window of the fog image.
    app.world_mut().get_mut::<Radar>(minimap).expect("radar").mode = RadarMode::Minimap {
        world_rect: [0.0, 0.0, 64.0, 64.0],
    };
    app.update();
    let quads = app.world().resource::<SharedUiRenderer>().lock().quads().to_vec();
    let fog = quads
        .iter()
        .find(|q| q.texture == TextureRef::Image(FOG))
        .expect("fog quad");
    assert_eq!(fog.uv, [0.5, 0.5, 1.0, 1.0]);
    // Removing the layer removes its node.
    app.world_mut().entity_mut(minimap).remove::<RadarFog>();
    app.update();
    let quads = app.world().resource::<SharedUiRenderer>().lock().quads().to_vec();
    assert!(!quads.iter().any(|q| q.texture == TextureRef::Image(FOG)));
}
