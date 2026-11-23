// SPDX-License-Identifier: MPL-2.0
//! Camera and letterbox math against hand-computed values (test-gap audit): offset viewports,
//! the matrix composition order, pixel snapping, integer-scale choice and the window-to-virtual
//! inverse. CPU only.

use regia_2d::{Camera2dView, OrthoCamera, PixelPerfect, Rect2};
use regia_core::math::{UVec2, Vec2};

fn close(a: Vec2, b: Vec2) -> bool {
    (a - b).abs().max_element() < 1e-3
}

/// Catches a viewport origin that is ignored (corners land at 0 instead of at the viewport edge)
/// or a Y flip applied on the wrong side of the viewport offset.
#[test]
fn display_rect_corners_land_on_the_edges_of_an_offset_viewport() {
    let cam = Camera2dView::from_display_rect(Rect2::new(100.0, 50.0, 320.0, 180.0));
    let viewport = Rect2::new(10.0, 20.0, 640.0, 360.0);
    // World +Y is up, screen +Y is down: the world's bottom-left is the viewport's bottom-left.
    assert!(close(
        cam.world_to_screen(Vec2::new(100.0, 50.0), viewport),
        Vec2::new(10.0, 380.0)
    ));
    assert!(close(
        cam.world_to_screen(Vec2::new(420.0, 230.0), viewport),
        Vec2::new(650.0, 20.0)
    ));
    assert!(close(
        cam.world_to_screen(Vec2::new(260.0, 140.0), viewport),
        Vec2::new(330.0, 200.0)
    ));
    assert!(close(
        cam.screen_to_world(Vec2::new(10.0, 20.0), viewport),
        Vec2::new(100.0, 230.0)
    ));
}

/// Catches the `offset` field being applied before the zoom (it is in clip units, applied last).
#[test]
fn offset_is_in_clip_units_applied_after_the_zoom() {
    let cam = Camera2dView {
        rotation: 0.0,
        zoom: Vec2::new(0.1, 0.1),
        target: Vec2::new(10.0, 0.0),
        offset: Vec2::new(0.5, 0.0),
    };
    let viewport = Rect2::new(0.0, 0.0, 100.0, 100.0);
    // The target sits at clip x = 0.5 -> three quarters of the viewport width.
    assert!(close(
        cam.world_to_screen(Vec2::new(10.0, 0.0), viewport),
        Vec2::new(75.0, 50.0)
    ));
    // One world unit is zoom 0.1 clip units = 5 px.
    assert!(close(
        cam.world_to_screen(Vec2::new(12.0, 0.0), viewport),
        Vec2::new(85.0, 50.0)
    ));
}

/// Catches rotation about the world origin instead of about `target`.
#[test]
fn rotation_pivots_on_the_target_not_the_origin() {
    let cam = Camera2dView {
        rotation: std::f32::consts::PI,
        zoom: Vec2::new(0.1, 0.1),
        target: Vec2::new(10.0, 0.0),
        offset: Vec2::ZERO,
    };
    let viewport = Rect2::new(0.0, 0.0, 100.0, 100.0);
    // (11, 0) is one unit right of the target; turned half a circle it lands one unit left.
    assert!(close(
        cam.world_to_screen(Vec2::new(11.0, 0.0), viewport),
        Vec2::new(45.0, 50.0)
    ));
    assert!(close(
        cam.world_to_screen(Vec2::new(10.0, 0.0), viewport),
        Vec2::new(50.0, 50.0)
    ));
}

/// Catches zoom axes being swapped or a non-square zoom collapsing to one factor.
#[test]
fn a_non_square_zoom_scales_each_axis_on_its_own() {
    let cam = Camera2dView {
        zoom: Vec2::new(0.5, 0.25),
        target: Vec2::new(4.0, 8.0),
        ..Camera2dView::default()
    };
    let viewport = Rect2::new(0.0, 0.0, 200.0, 100.0);
    // +2 world x = clip +1 (right edge); +4 world y = clip +1 (top edge).
    assert!(close(
        cam.world_to_screen(Vec2::new(6.0, 8.0), viewport),
        Vec2::new(200.0, 50.0)
    ));
    assert!(close(
        cam.world_to_screen(Vec2::new(4.0, 12.0), viewport),
        Vec2::new(100.0, 0.0)
    ));
}

/// Catches an ortho view that forgets the centre or the pixels-per-unit factor.
#[test]
fn ortho_view_centres_on_its_target_and_scales_by_pixels_per_unit() {
    let cam = OrthoCamera {
        center: Vec2::new(50.0, -20.0),
        pixels_per_unit: 3.0,
        rotation: 0.0,
    };
    let size = Vec2::new(300.0, 150.0);
    let view = cam.view(size);
    assert!(close(view.zoom, Vec2::new(2.0 * 3.0 / 300.0, 2.0 * 3.0 / 150.0)));
    let viewport = Rect2::new(0.0, 0.0, size.x, size.y);
    assert!(close(
        view.world_to_screen(Vec2::new(50.0, -20.0), viewport),
        Vec2::new(150.0, 75.0)
    ));
    // One unit right and one unit down is three pixels right and three pixels down.
    assert!(close(
        view.world_to_screen(Vec2::new(51.0, -21.0), viewport),
        Vec2::new(153.0, 78.0)
    ));
    let rect = cam.visible_rect(size);
    assert!(close(rect.min, Vec2::new(0.0, -45.0)));
    assert!(close(rect.size, Vec2::new(100.0, 50.0)));
}

/// Catches `visible_rect` and `view` disagreeing about what is on screen (tiles would be culled
/// while still visible, or drawn when not).
#[test]
fn visible_rect_corners_map_to_the_screen_corners_of_the_view() {
    let cam = OrthoCamera {
        center: Vec2::new(-7.5, 12.25),
        pixels_per_unit: 2.5,
        rotation: 0.0,
    };
    let size = Vec2::new(640.0, 360.0);
    let rect = cam.visible_rect(size);
    let view = cam.view(size);
    let viewport = Rect2::new(0.0, 0.0, size.x, size.y);
    assert!(close(
        view.world_to_screen(rect.min, viewport),
        Vec2::new(0.0, size.y)
    ));
    assert!(close(
        view.world_to_screen(rect.min + rect.size, viewport),
        Vec2::new(size.x, 0.0)
    ));
}

/// Catches a non-positive zoom producing a negative, infinite or NaN camera scale (a mirrored or
/// invisible world) instead of the documented clamp.
#[test]
fn a_non_positive_pixels_per_unit_keeps_a_finite_non_negative_zoom() {
    for ppu in [0.0, -3.0] {
        let cam = OrthoCamera {
            pixels_per_unit: ppu,
            ..OrthoCamera::default()
        };
        let view = cam.view(Vec2::new(320.0, 180.0));
        assert!(
            view.zoom.is_finite() && view.zoom.min_element() >= 0.0,
            "{ppu}: {view:?}"
        );
    }
    // A zero-sized output is treated as one pixel, not divided by.
    let view = OrthoCamera::default().view(Vec2::ZERO);
    assert_eq!(view.zoom, Vec2::new(2.0, 2.0));
}

/// Catches snapping that floors or truncates instead of rounding the view edge to the nearest
/// pixel, and a snapped camera that is not stable when snapped again.
#[test]
fn snapping_rounds_the_edge_to_the_nearest_pixel_and_is_idempotent() {
    let cam = OrthoCamera {
        center: Vec2::new(0.6, 0.0),
        pixels_per_unit: 1.0,
        rotation: 0.0,
    };
    // Edge = 0.6 - 50 = -49.4 -> -49 (nearest); a floor would give -50 and a centre of 0.
    let snapped = cam.snapped(UVec2::new(100, 100));
    assert!(close(snapped.center, Vec2::new(1.0, 0.0)), "{:?}", snapped.center);
    assert_eq!(snapped.snapped(UVec2::new(100, 100)), snapped);
}

/// Catches snapping that does not make integer world positions land on whole screen pixels (the
/// shimmer it exists to prevent), at a non-trivial zoom.
#[test]
fn after_snapping_integer_world_points_hit_whole_pixels() {
    let cam = OrthoCamera {
        center: Vec2::new(10.3, 5.7),
        pixels_per_unit: 4.0,
        rotation: 0.0,
    };
    let size = UVec2::new(320, 180);
    let snapped = cam.snapped(size);
    // Edge = (41.2, 22.8) - (160, 90) = (-118.8, -67.2) -> (-119, -67) -> centre (41, 23) / 4.
    assert!(
        close(snapped.center, Vec2::new(10.25, 5.75)),
        "{:?}",
        snapped.center
    );
    let view = snapped.view(Vec2::new(320.0, 180.0));
    let viewport = Rect2::new(0.0, 0.0, 320.0, 180.0);
    // 3 world units from the left edge origin: x = 160 + (3 - 10.25) * 4 = 131, y = 90 - (7 - 5.75) * 4 = 85.
    assert!(close(
        view.world_to_screen(Vec2::new(3.0, 7.0), viewport),
        Vec2::new(131.0, 85.0)
    ));
    assert!(close(
        view.world_to_screen(Vec2::new(4.0, 7.0), viewport),
        Vec2::new(135.0, 85.0)
    ));
}

/// Catches the integer scale taking the wider axis (the image would overflow the short axis) and
/// the margin not being split evenly.
#[test]
fn letterbox_scale_is_limited_by_the_tighter_axis() {
    let pp = PixelPerfect::new(320, 180);
    let lb = pp.letterbox(UVec2::new(1000, 400));
    // 1000 / 320 = 3, 400 / 180 = 2: the scale is 2, the image 640 x 360.
    assert_eq!((lb.scale, lb.width, lb.height), (2, 640, 360));
    assert_eq!((lb.x, lb.y), (180, 20));
    let exact = pp.letterbox(UVec2::new(320, 180));
    assert_eq!(
        (exact.scale, exact.x, exact.y, exact.width, exact.height),
        (1, 0, 0, 320, 180)
    );
    // One pixel short on one axis drops the scale to 1 but keeps the full image inside.
    let tight = pp.letterbox(UVec2::new(639, 360));
    assert_eq!((tight.scale, tight.width, tight.height), (1, 320, 180));
}

/// Catches a window-to-virtual mapping that is off by a pixel at the letterbox edges: every
/// virtual pixel centre must come back as that pixel plus a half, and the bars as `None`.
#[test]
fn window_to_virtual_maps_pixel_centres_and_rejects_the_bars() {
    let pp = PixelPerfect::new(320, 180);
    let window = UVec2::new(1366, 768);
    // Scale 4, image at (43, 24).
    for (vx, vy) in [(0u32, 0u32), (1, 0), (160, 90), (319, 179)] {
        let point = Vec2::new(43.0 + vx as f32 * 4.0 + 2.0, 24.0 + vy as f32 * 4.0 + 2.0);
        assert_eq!(
            pp.window_to_virtual(window, point),
            Some(Vec2::new(vx as f32 + 0.5, vy as f32 + 0.5)),
            "virtual pixel ({vx}, {vy})"
        );
    }
    // The last window pixel of the image is inside, the next one is a bar.
    assert!(
        pp.window_to_virtual(window, Vec2::new(43.0 + 1279.0, 24.0))
            .is_some()
    );
    assert!(
        pp.window_to_virtual(window, Vec2::new(43.0 + 1280.0, 24.0))
            .is_none()
    );
    assert!(pp.window_to_virtual(window, Vec2::new(42.0, 24.0)).is_none());
    assert!(pp.window_to_virtual(window, Vec2::new(43.0, 23.0)).is_none());
    assert!(
        pp.window_to_virtual(window, Vec2::new(43.0, 24.0 + 720.0))
            .is_none()
    );
}
