// SPDX-License-Identifier: MPL-2.0
//! Logical placement follows live viewport geometry without a graphics device.

use regia_app::prelude::*;
use regia_framework::perf_overlay::{
    OverlayCorner, PerfOverlayPlugin, PerfOverlayState, PerformanceOverlayMode, PerformanceOverlayPlacement,
    set_override,
};
use regia_ui::{ComputedNode, UiPlugin};

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(regia_ui::text::TextSystem::embedded_only());
    app.add_plugins(UiPlugin::code_only())
        .add_plugins(PerfOverlayPlugin);
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    set_override(app.world_mut(), Some(PerformanceOverlayMode::Compact));
    app
}

#[test]
fn overlay_placement_tracks_four_corners_and_live_resize() {
    let mut app = app();
    app.update();
    for (width, height, dpi) in [(1280.0, 720.0, 1.0), (2560.0, 1440.0, 2.0)] {
        {
            let mut scales = app.world_mut().resource_mut::<regia_ui::scale::UiScales>();
            scales.window_size = [width, height];
            scales.scale_factor = dpi;
        }
        for corner in [
            OverlayCorner::TopLeft,
            OverlayCorner::TopRight,
            OverlayCorner::BottomLeft,
            OverlayCorner::BottomRight,
        ] {
            app.insert_resource(PerformanceOverlayPlacement {
                corner,
                offset: [24.0, 32.0],
            });
            app.update();
            let root = app.world().resource::<PerfOverlayState>().root().expect("root");
            let panel = app.world().get::<Children>(root).expect("panel child")[0];
            let node = app.world().get::<ComputedNode>(panel).expect("laid out");
            let right = matches!(corner, OverlayCorner::TopRight | OverlayCorner::BottomRight);
            let bottom = matches!(corner, OverlayCorner::BottomLeft | OverlayCorner::BottomRight);
            let x = if right {
                width - 24.0 * node.scale - node.rect[2]
            } else {
                24.0 * node.scale
            };
            let y = if bottom {
                height - 32.0 * node.scale - node.rect[3]
            } else {
                32.0 * node.scale
            };
            assert!(
                (node.rect[0] - x).abs() <= 1.0,
                "{corner:?}: {node:?}, expected x={x}"
            );
            assert!(
                (node.rect[1] - y).abs() <= 1.0,
                "{corner:?}: {node:?}, expected y={y}"
            );
        }
    }
}

#[test]
fn overlay_placement_sanitizes_invalid_offsets() {
    let p = PerformanceOverlayPlacement {
        corner: OverlayCorner::BottomRight,
        offset: [f32::NAN, -10.0],
    };
    assert_eq!(p.normalized().offset, [10.0, 10.0]);
    assert_eq!(p.normalized().corner, OverlayCorner::BottomRight);
}
