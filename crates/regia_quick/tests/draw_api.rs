// SPDX-License-Identifier: MPL-2.0
//! The draw API end to end on the CPU: builders → commands → tessellation → batch and stats,
//! and the plugin inside an app (headless renderer, no GPU needed).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::f32::consts::{FRAC_PI_2, PI};

use regia_platform::{PrimaryWindow, Window};
use regia_quick::frame::{FrameParams, QuickRenderer};
use regia_quick::prelude::*;
use regia_quick::{DrawFrozen, Quick, QuickDrawStats, QuickOverlay, RunMode};
use regia_render::{ExtractedFrame, ImmediateBatch, ImmediateBlend, ImmediateTexture, RenderPlugin};
use regia_ui::Localization;
use regia_ui::text::TextSystem;

const PARAMS: FrameParams = FrameParams {
    size: Vec2::new(640.0, 360.0),
    scale_factor: 1.0,
};

fn build(draw: &Draw) -> (ImmediateBatch, QuickDrawStats) {
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    let stats = renderer.build(draw, PARAMS, None, &mut batch);
    (batch, stats)
}

fn area(batch: &ImmediateBatch) -> f32 {
    let v = batch.vertices();
    batch
        .indices()
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let [a, b, c] = t.map(|i| Vec2::new(v[i as usize].position[0], v[i as usize].position[1]));
            0.5 * (b - a).perp_dot(c - a).abs()
        })
        .sum()
}

fn near(a: f32, b: f32, eps: f32) -> bool {
    (a - b).abs() <= eps
}

#[test]
fn rect_takes_the_fast_path_and_reports_bounds() {
    let draw = Draw::new();
    draw.rect().x_y(10.0, 20.0).w_h(40.0, 20.0).color(Color::RED);
    let (batch, stats) = build(&draw);
    assert_eq!(batch.vertices().len(), 4, "four vertices, no tessellator");
    assert_eq!(stats.primitives, 1);
    assert_eq!(stats.by_kind.get("rect"), Some(&1));
    assert_eq!(stats.bounds, Some([-10.0, 10.0, 30.0, 30.0]));
    assert_eq!(batch.vertices()[0].color, Color::RED.to_array());
    assert!(draw.is_empty(), "build drains the draw");
}

#[test]
fn derived_draws_compose_transforms() {
    let draw = Draw::new();
    draw.x(100.0).rotate(FRAC_PI_2).rect().w_h(10.0, 2.0);
    let (_, stats) = build(&draw);
    let b = stats.bounds.unwrap();
    assert!(near(b[0], 99.0, 1e-4) && near(b[2], 101.0, 1e-4), "{b:?}");
    assert!(near(b[1], -5.0, 1e-4) && near(b[3], 5.0, 1e-4), "{b:?}");
    // The parent draw is untouched.
    draw.rect().w_h(2.0, 2.0);
    assert_eq!(build(&draw).1.bounds, Some([-1.0, -1.0, 1.0, 1.0]));
}

#[test]
fn ellipse_fill_and_stroke_match_their_areas() {
    let draw = Draw::new();
    draw.ellipse().radius(50.0);
    let (fill, _) = build(&draw);
    let exact = PI * 50.0 * 50.0;
    // The polygon stays within 0.1 px of the curve: about 0.3 % of the area at r = 50.
    assert!(
        (area(&fill) - exact).abs() / exact < 5e-3,
        "fill area {}",
        area(&fill)
    );

    draw.ellipse()
        .radius(50.0)
        .no_fill()
        .stroke(Color::WHITE)
        .stroke_weight(4.0);
    let (ring, _) = build(&draw);
    let exact_ring = PI * (52.0f32.powi(2) - 48.0f32.powi(2));
    assert!(
        (area(&ring) - exact_ring).abs() / exact_ring < 5e-3,
        "ring area {}",
        area(&ring)
    );
}

#[test]
fn lines_have_their_caps() {
    let draw = Draw::new();
    draw.line()
        .start(Vec2::ZERO)
        .end(Vec2::new(100.0, 0.0))
        .weight(10.0);
    let (butt, _) = build(&draw);
    assert!(near(area(&butt), 1000.0, 1e-2));
    draw.line()
        .start(Vec2::ZERO)
        .end(Vec2::new(100.0, 0.0))
        .weight(10.0)
        .caps_square();
    assert!(near(area(&build(&draw).0), 1100.0, 1e-2));
    draw.line()
        .start(Vec2::ZERO)
        .end(Vec2::new(100.0, 0.0))
        .weight(10.0)
        .caps_round();
    let round = area(&build(&draw).0);
    assert!((round - (1000.0 + PI * 25.0)).abs() < 4.0, "{round}");
    // Mixed caps fall back to lyon.
    draw.line()
        .start(Vec2::ZERO)
        .end(Vec2::new(100.0, 0.0))
        .weight(10.0)
        .start_cap_round()
        .end_cap_butt();
    let mixed = area(&build(&draw).0);
    assert!((mixed - (1000.0 + PI * 12.5)).abs() < 2.0, "{mixed}");
}

#[test]
fn paths_polygons_meshes_and_arrows_tessellate() {
    let draw = Draw::new();
    let zigzag = (0..10).map(|i| Vec2::new(i as f32 * 20.0, if i % 2 == 0 { 0.0 } else { 30.0 }));
    draw.polyline().weight(3.0).join_round().points(zigzag);
    draw.path()
        .fill()
        .points_closed([Vec2::ZERO, Vec2::X * 10.0, Vec2::Y * 10.0]);
    draw.tri().points(Vec2::ZERO, Vec2::X * 10.0, Vec2::Y * 10.0);
    draw.quad()
        .points(Vec2::ZERO, Vec2::X, Vec2::ONE, Vec2::Y)
        .w_h(20.0, 20.0);
    draw.arrow()
        .start(Vec2::ZERO)
        .end(Vec2::new(50.0, 0.0))
        .weight(2.0);
    draw.mesh().indexed_colored(
        [
            (Vec3::ZERO, Color::RED),
            (Vec3::X * 10.0, Color::GREEN),
            (Vec3::Y * 10.0, Color::BLUE),
        ],
        [0, 1, 2],
    );
    let (batch, stats) = build(&draw);
    for kind in ["path", "tri", "quad", "arrow", "mesh"] {
        assert!(
            stats.by_kind.contains_key(kind),
            "{kind} missing: {:?}",
            stats.by_kind
        );
    }
    assert_eq!(stats.primitives, 6);
    assert!(stats.triangles > 20, "{}", stats.triangles);
    assert!(area(&batch) > 0.0);
    assert!(
        batch
            .indices()
            .iter()
            .all(|i| (*i as usize) < batch.vertices().len())
    );
}

#[test]
fn polygon_covers_its_own_area() {
    // A 40×40 square drawn alone: the triangles cover exactly 1600 units².
    let draw = Draw::new();
    draw.polygon()
        .points([
            Vec2::new(0.0, 0.0),
            Vec2::new(40.0, 0.0),
            Vec2::new(40.0, 40.0),
            Vec2::new(0.0, 40.0),
        ])
        .color(Color::GREEN);
    let (batch, stats) = build(&draw);
    assert_eq!(stats.by_kind.get("polygon"), Some(&1));
    assert_eq!(stats.primitives, 1);
    let covered = area(&batch);
    assert!(near(covered, 1600.0, 1e-2), "{covered}");
}

#[test]
fn blend_scissor_and_texture_split_draw_calls() {
    let draw = Draw::new();
    for i in 0..100 {
        draw.ellipse().x(i as f32).radius(3.0);
    }
    draw.blend(BlendMode::Additive).rect().w_h(10.0, 10.0);
    draw.scissor(geom::Rect::from_w_h(100.0, 100.0)).rect();
    draw.rect().texture(QuickImage(7));
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    let stats = renderer.build(&draw, PARAMS, None, &mut batch);
    assert_eq!(stats.draw_calls, 4, "100 ellipses share one call");
    let draws = batch.draws();
    assert_eq!(draws[1].state.blend, ImmediateBlend::Additive);
    // 100×100 centred on a 640×360 window.
    assert_eq!(draws[2].state.scissor, Some([270, 130, 100, 100]));
    assert_eq!(draws[3].state.texture, ImmediateTexture::Image(7));
}

#[test]
fn background_and_text_reach_the_stats_and_the_glyph_atlas() {
    let draw = Draw::new();
    draw.background().color(Color::BLUE);
    draw.text("Hello quick").font_size(24).color(Color::RED);
    let mut fonts = TextSystem::embedded_only();
    let localization = Localization::default();
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    let stats = renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(stats.background, Some(Color::BLUE.to_array()));
    assert_eq!(stats.texts, vec!["Hello quick".to_string()]);
    let masks = batch
        .draws()
        .iter()
        .filter(|d| matches!(d.state.texture, ImmediateTexture::Mask(_)))
        .count();
    assert!(masks >= 1, "glyph quads draw from a mask page");
    // 10 visible glyphs → 10 quads after the 2 background triangles.
    assert_eq!(stats.triangles, 2 + 10 * 2);
    // Centred text: its bounds straddle the origin (background excluded: check glyph vertices).
    let glyph_x: Vec<f32> = batch.vertices()[4..].iter().map(|v| v.position[0]).collect();
    let (min, max) = glyph_x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), x| (a.min(*x), b.max(*x)));
    assert!(
        min < -40.0 && max > 40.0 && (min + max).abs() < 12.0,
        "{min}..{max}"
    );
    // Vertically centred in the full-window rect: the glyph rows sit around y = 0, not at an
    // edge (top alignment would put them near -168), and carry the requested colour.
    let glyph_y: Vec<f32> = batch.vertices()[4..].iter().map(|v| v.position[1]).collect();
    let (ymin, ymax) = glyph_y
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), y| (a.min(*y), b.max(*y)));
    assert!(
        ymin < 0.0 + 12.0 && ymax > -12.0 && (ymin + ymax).abs() < 24.0,
        "{ymin}..{ymax}"
    );
    assert!(
        batch.vertices()[4..]
            .iter()
            .all(|v| v.color == Color::RED.to_array()),
        "glyph vertices carry the text colour"
    );
    let pages = renderer.take_images();
    assert!(!pages.is_empty(), "the atlas page is handed to the GPU renderer");
    assert!(renderer.take_images().is_empty(), "pages are uploaded once");
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(RenderPlugin { headless: true })
        .add_plugins(QuickPlugin);
    app.finish_plugins();
    app
}

fn scene(draw: Res<Draw>, draw3d: Res<Draw3d>) {
    draw.rect().w_h(50.0, 50.0).color(Color::RED);
    draw.text("score 7");
    draw3d.cube(Vec3::ZERO, Vec3::ONE).color(Color::GREEN);
    draw3d.sphere(Vec3::X * 2.0, 0.5);
    draw3d.grid(Vec3::ZERO, 4, 1.0, Color::RED, Color::GRAY);
}

#[test]
fn plugin_extracts_every_frame_and_freezes_on_request() {
    let mut app = app();
    app.add_systems(Update, scene);
    app.update();
    {
        let stats = app.world().resource::<QuickDrawStats>();
        assert_eq!(stats.frame, 1);
        assert_eq!(stats.primitives, 2);
        assert_eq!(stats.texts, vec!["score 7".to_string()]);
        assert_eq!(stats.primitives_3d, 2);
        assert_eq!(stats.lines_3d, 10);
    }
    assert!(!app.world().resource::<QuickOverlay>().lock().batch.is_empty());
    assert!(
        app.world().resource::<Draw>().is_empty(),
        "drawings last one frame"
    );
    // The app spawns no mesh entities: every extracted mesh is a quick drawing.
    let quick_meshes = app.world().resource::<ExtractedFrame>().meshes.len();
    assert_eq!(quick_meshes, 2, "Draw3d instances join the forward path");
    assert!(
        !app.world().resource::<ExtractedFrame>().gizmos.is_empty(),
        "3D lines become gizmo lines"
    );

    // Frozen: the view still runs, but nothing is drained or rebuilt.
    app.world_mut().resource_mut::<DrawFrozen>().0 = true;
    app.update();
    assert_eq!(app.world().resource::<QuickDrawStats>().frame, 1);
    assert!(!app.world().resource::<QuickOverlay>().lock().batch.is_empty());
    // `extract_frame` clears the meshes each frame, so a frozen frame must resubmit the last
    // drained drawings. The view redraws the same two primitives here; they stay buffered and
    // are not what the held frame shows.
    assert_eq!(
        app.world().resource::<ExtractedFrame>().meshes.len(),
        2,
        "a frozen frame still shows the held 3D drawings"
    );
}

#[test]
fn nothing_drawn_costs_nothing() {
    let mut app = app();
    app.update();
    let stats = app.world().resource::<QuickDrawStats>();
    assert_eq!((stats.primitives, stats.vertices, stats.draw_calls), (0, 0, 0));
    assert!(app.world().resource::<QuickOverlay>().lock().batch.is_empty());
    assert!(app.world().resource::<ExtractedFrame>().meshes.is_empty());
}

#[test]
fn consecutive_meshes_keep_their_own_indices() {
    let draw = Draw::new();
    draw.mesh().points([Vec3::ZERO, Vec3::X * 10.0, Vec3::Y * 10.0]);
    draw.mesh().points([Vec3::ZERO, Vec3::X * 20.0, Vec3::Y * 20.0]);
    draw.mesh()
        .indexed([Vec3::ZERO, Vec3::X * 5.0, Vec3::Y * 5.0], [0, 1, 2]);
    // A triangle that names a vertex the mesh does not have is dropped, not drawn.
    draw.mesh()
        .indexed([Vec3::ZERO, Vec3::X * 5.0, Vec3::Y * 5.0], [0, 1, 7]);
    let (batch, stats) = build(&draw);
    assert_eq!(stats.primitives, 4);
    // The dropped mesh has no indices, so the frame skips it and submits none of its vertices.
    assert_eq!(batch.vertices().len(), 9);
    assert_eq!(batch.indices().len(), 9);
    let n = batch.vertices().len() as u32;
    assert!(batch.indices().iter().all(|&i| i < n), "{:?}", batch.indices());
    // 50 + 200 + 12.5 units² of triangles.
    assert!(near(area(&batch), 262.5, 1e-3), "{}", area(&batch));
}

#[test]
fn huge_mesh_indices_are_dropped_without_overflow() {
    let draw = Draw::new();
    draw.mesh().points([Vec3::ZERO, Vec3::X * 3.0, Vec3::Y * 3.0]);
    // Offset by the first mesh's vertices, an index near usize::MAX would wrap if added unchecked.
    draw.mesh()
        .indexed([Vec3::ZERO, Vec3::X * 2.0, Vec3::Y * 2.0], [0, 1, usize::MAX]);
    let (batch, _) = build(&draw);
    assert_eq!(batch.indices().len(), 3);
    let n = batch.vertices().len() as u32;
    assert!(batch.indices().iter().all(|&i| i < n), "{:?}", batch.indices());
}

#[test]
fn iterators_may_draw_while_a_primitive_is_built() {
    let draw = Draw::new();
    draw.mesh().points((0..3).map(|i| {
        draw.ellipse().radius(4.0);
        Vec3::new(i as f32, 0.0, 0.0)
    }));
    draw.polygon().points((0..3).map(|i| {
        draw.rect().w_h(2.0, 2.0);
        Vec2::new(i as f32, (i % 2) as f32)
    }));
    draw.path().stroke().points((0..3).map(|i| {
        draw.rect().w_h(2.0, 2.0);
        Vec2::new(i as f32, 0.0)
    }));
    let (_, stats) = build(&draw);
    assert_eq!(stats.primitives, 12);
}

#[test]
fn rect_stroke_without_a_colour_uses_the_stroke_colour() {
    let draw = Draw::new();
    draw.ellipse().radius(10.0).no_fill().stroke_weight(4.0);
    let (reference, _) = build(&draw);
    let want = reference.vertices()[0].color;
    draw.rect().w_h(80.0, 40.0).no_fill().stroke_weight(4.0);
    let (batch, _) = build(&draw);
    assert!(!batch.vertices().is_empty());
    assert!(batch.vertices().iter().all(|v| v.color == want));
    assert_ne!(
        want,
        Color::WHITE.to_array(),
        "default stroke is not the fill colour"
    );
}

/// A zero-width stroke covers no pixels: it adds no vertices (lyon would emit NaN positions).
#[test]
fn zero_weight_strokes_add_no_vertices() {
    let draw = Draw::new();
    draw.polyline()
        .weight(0.0)
        .points([Vec2::ZERO, Vec2::new(40.0, 10.0), Vec2::new(80.0, 0.0)]);
    let (batch, _) = build(&draw);
    assert!(batch.vertices().is_empty(), "{}", batch.vertices().len());
}

#[test]
fn degenerate_extents_and_arrows_stay_finite() {
    let draw = Draw::new();
    draw.tri().points(Vec2::ZERO, Vec2::Y, Vec2::Y * 2.0).w(10.0);
    draw.arrow()
        .start(Vec2::ZERO)
        .end(Vec2::new(50.0, 0.0))
        .weight(0.0);
    let (batch, stats) = build(&draw);
    assert!(
        batch
            .vertices()
            .iter()
            .all(|v| v.position.iter().all(|c| c.is_finite()))
    );
    assert!(stats.bounds.is_some_and(|b| b.iter().all(|c| c.is_finite())));
}

#[test]
fn ellipse_resolution_is_capped() {
    let draw = Draw::new();
    draw.ellipse().radius(10.0).resolution(1e10);
    let (batch, _) = build(&draw);
    assert_eq!(batch.vertices().len(), 1025);
}

#[test]
fn flat_ellipse_stroke_is_offset_along_the_normal() {
    let draw = Draw::new();
    draw.ellipse()
        .w_h(200.0, 20.0)
        .resolution(8.0)
        .no_fill()
        .stroke_weight(10.0);
    let (batch, _) = build(&draw);
    let v = batch.vertices();
    assert_eq!(v.len(), 16);
    for pair in v.as_chunks::<2>().0 {
        let [o, i] = pair.each_ref().map(|p| Vec2::new(p.position[0], p.position[1]));
        let d = o - i;
        let mid = (o + i) * 0.5;
        let tangent = Vec2::new(-mid.y * 100.0 / 10.0, mid.x * 10.0 / 100.0);
        assert!(
            d.dot(tangent).abs() <= 1e-3 * d.length() * tangent.length().max(1e-6),
            "{d:?} not normal to the curve at {mid:?}"
        );
        assert!(near(d.length(), 10.0, 1e-3));
    }
}

#[test]
fn point_meshes_with_a_partial_triangle_do_not_shift_the_next_draw() {
    let draw = Draw::new();
    draw.mesh()
        .points([Vec3::ZERO, Vec3::X * 10.0, Vec3::Y * 10.0, Vec3::ONE]);
    draw.rect().w_h(10.0, 10.0);
    let (batch, _) = build(&draw);
    assert_eq!(batch.indices().len() % 3, 0);
    // The lone fourth point is not drawn, so the rect keeps both of its own triangles.
    assert!(near(area(&batch), 50.0 + 100.0, 1e-3), "area {}", area(&batch));
}

/// Which cube the always-running view draws while the frozen-frame test steps.
#[derive(Resource, Default)]
struct HeldCubePhase(u32);

fn mesh_x(app: &App) -> f32 {
    app.world()
        .resource::<ExtractedFrame>()
        .meshes
        .first()
        .map(|mesh| mesh.model[3][0])
        .unwrap_or(f32::NAN)
}

#[test]
fn loop_once_holds_the_3d_cube_and_grid() {
    let mut game = App::new();
    game.add_plugins(RenderPlugin { headless: true })
        .add_plugins(QuickPlugin)
        .add_plugins(
            regia_quick::sketch(|q: &Quick| {
                q.draw_3d().cube(Vec3::ZERO, Vec3::ONE);
                q.draw_3d().grid(Vec3::ZERO, 4, 1.0, Color::RED, Color::GRAY);
            })
            .run_mode(RunMode::loop_once()),
        );
    game.finish_plugins();
    game.update();
    assert_eq!(game.world().resource::<QuickDrawStats>().primitives_3d, 1);
    assert_eq!(game.world().resource::<QuickDrawStats>().lines_3d, 10);
    // The budget is spent: `view` does not run, and the last 3D frame must stay submitted.
    game.update();
    assert!(
        game.world().resource::<DrawFrozen>().0,
        "loop_once holds the frame"
    );
    let (meshes, mesh_x, segments) = {
        let frame = game.world().resource::<ExtractedFrame>();
        (
            frame.meshes.len(),
            frame.meshes.first().map(|mesh| mesh.model[3][0]),
            frame.gizmos.segment_count(),
        )
    };
    assert_eq!(meshes, 1, "the held cube is still in the extracted frame");
    assert_eq!(mesh_x, Some(0.0));
    assert_eq!(segments, 10, "the quick gizmo group still contains the grid");
    let stats = game.world().resource::<QuickDrawStats>();
    assert_eq!((stats.primitives_3d, stats.lines_3d), (1, 10));
}

#[test]
fn a_frozen_frame_keeps_the_first_cube_then_submits_every_draw_from_the_hold() {
    let mut game = app();
    game.insert_resource(HeldCubePhase(0));
    game.add_systems(
        Update,
        |draw3d: Res<Draw3d>, phase: Res<HeldCubePhase>| match phase.0 {
            0 => {
                draw3d.cube(Vec3::ZERO, Vec3::ONE);
            }
            1 => {
                draw3d.cube(Vec3::new(5.0, 0.0, 0.0), Vec3::ONE);
            }
            _ => {}
        },
    );
    game.update();
    assert_eq!(game.world().resource::<ExtractedFrame>().meshes.len(), 1);
    assert_eq!(mesh_x(&game), 0.0);

    game.world_mut().resource_mut::<DrawFrozen>().0 = true;
    game.world_mut().resource_mut::<HeldCubePhase>().0 = 1;
    game.update();
    assert_eq!(
        game.world().resource::<ExtractedFrame>().meshes.len(),
        1,
        "the held frame is still one cube"
    );
    assert_eq!(mesh_x(&game), 0.0, "drawings made while frozen are not shown yet");
    // A second frozen frame used to append again, so one cube per frame became two.
    game.update();
    assert_eq!(game.world().resource::<ExtractedFrame>().meshes.len(), 1);
    assert_eq!(mesh_x(&game), 0.0);

    game.world_mut().resource_mut::<DrawFrozen>().0 = false;
    game.world_mut().resource_mut::<HeldCubePhase>().0 = 2;
    game.update();
    let xs: Vec<f32> = game
        .world()
        .resource::<ExtractedFrame>()
        .meshes
        .iter()
        .map(|mesh| mesh.model[3][0])
        .collect();
    assert_eq!(
        xs.len(),
        2,
        "both cubes drawn during the hold are submitted once on the next live frame"
    );
    assert!(xs.iter().all(|x| (*x - 5.0).abs() < 1e-4), "{xs:?}");
}

#[test]
fn frozen_frame_keeps_logical_projection_when_the_window_scale_changes() {
    let mut game = app();
    let window = game
        .world_mut()
        .spawn((
            Window {
                physical_size: (800, 600),
                scale_factor: 1.0,
                ..Window::default()
            },
            PrimaryWindow,
        ))
        .id();
    game.add_systems(Update, |draw: Res<Draw>| {
        draw.rect().x_y(100.0, 0.0).w_h(2.0, 2.0);
        draw.scissor(geom::Rect::from_x_y_w_h(0.0, 0.0, 100.0, 50.0))
            .rect()
            .w_h(10.0, 10.0);
    });
    game.update();
    let baked: Vec<[u32; 4]> = {
        let state = game.world().resource::<QuickOverlay>().lock();
        assert_eq!(state.logical_size, Vec2::new(800.0, 600.0));
        assert_eq!(state.built_physical, (800, 600));
        assert_eq!(state.scale_factor, 1.0);
        let clip_x = state
            .clip_from_local_for_target(800, 600)
            .transform_point3(Vec3::new(100.0, 0.0, 0.0))
            .x;
        assert!((clip_x - 0.25).abs() < 1e-5, "built clip x {clip_x}");
        state
            .batch
            .draws()
            .iter()
            .filter_map(|draw| draw.state.scissor)
            .collect()
    };
    assert!(!baked.is_empty(), "the scissored rect bakes a physical scissor");

    game.world_mut().resource_mut::<DrawFrozen>().0 = true;
    {
        let mut window = game
            .world_mut()
            .get_mut::<Window>(window)
            .expect("primary window");
        window.physical_size = (1600, 1200);
        window.scale_factor = 2.0;
    }
    game.update();
    let state = game.world().resource::<QuickOverlay>().lock();
    assert_eq!(
        state.scale_factor, 1.0,
        "the held batch keeps the scale it was built for"
    );
    assert_eq!(state.logical_size, Vec2::new(800.0, 600.0));
    let clip_x = state
        .clip_from_local_for_target(1600, 1200)
        .transform_point3(Vec3::new(100.0, 0.0, 0.0))
        .x;
    assert!(
        (clip_x - 0.25).abs() < 1e-5,
        "clip x {clip_x}; the new target divided by the stale scale is 0.125"
    );
    let scaled: Vec<[u32; 4]> = state
        .scissors_for_target(1600, 1200)
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(scaled.len(), baked.len());
    for (before, after) in baked.iter().zip(&scaled) {
        assert_eq!(
            *after,
            [before[0] * 2, before[1] * 2, before[2] * 2, before[3] * 2],
            "scissors were baked for 800×600 and the target is now 1600×1200"
        );
    }
}

#[derive(Resource)]
struct DrawGate(bool);

fn cube_and_grid(gate: Res<DrawGate>, draw3d: Res<Draw3d>) {
    if !gate.0 {
        return;
    }
    draw3d.cube(Vec3::ZERO, Vec3::ONE);
    draw3d.grid(Vec3::ZERO, 4, 1.0, Color::RED, Color::GRAY);
}

/// A frozen frame whose view does not run keeps the last 3D meshes and lines.
#[test]
fn a_frozen_frame_keeps_draw3d_when_the_view_does_not_run() {
    let mut app = app();
    app.insert_resource(DrawGate(true));
    app.add_systems(Update, cube_and_grid);
    app.update();
    {
        let stats = app.world().resource::<QuickDrawStats>();
        assert_eq!(stats.primitives_3d, 1);
        assert_eq!(stats.lines_3d, 10);
    }
    assert_eq!(app.world().resource::<ExtractedFrame>().meshes.len(), 1);
    app.world_mut().resource_mut::<DrawGate>().0 = false;
    app.world_mut().resource_mut::<DrawFrozen>().0 = true;
    app.update();
    let stats = app.world().resource::<QuickDrawStats>();
    assert_eq!(stats.primitives_3d, 1, "the held frame's cube is still counted");
    assert_eq!(stats.lines_3d, 10, "the held grid is still counted");
    assert_eq!(
        app.world().resource::<ExtractedFrame>().meshes.len(),
        1,
        "the cube is re-extracted, not dropped or doubled"
    );
    assert!(
        !app.world().resource::<ExtractedFrame>().gizmos.is_empty(),
        "the held grid lines are re-submitted"
    );
}

fn max_abs_y(batch: &ImmediateBatch) -> f32 {
    batch
        .vertices()
        .iter()
        .map(|v| v.position[1].abs())
        .fold(0.0, f32::max)
}

/// `head_width` is the distance from the centerline to each corner (full base is twice that).
#[test]
fn arrow_head_width_is_the_distance_from_the_centerline() {
    let draw = Draw::new();
    draw.arrow()
        .points(Vec2::ZERO, Vec2::new(100.0, 0.0))
        .weight(10.0);
    let (batch, _) = build(&draw);
    // Default distance is weight * 2, so the corners sit at |y| = 20 and the full base is 40.
    assert!(near(max_abs_y(&batch), 20.0, 1e-3), "{}", max_abs_y(&batch));

    let draw = Draw::new();
    draw.arrow()
        .points(Vec2::ZERO, Vec2::new(100.0, 0.0))
        .weight(10.0)
        .head_width(10.0);
    let (batch, _) = build(&draw);
    assert!(near(max_abs_y(&batch), 10.0, 1e-3), "{}", max_abs_y(&batch));
}

/// Distance from `p` to the segment `a`–`b`.
fn dist_to_segment(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let len2 = ab.dot(ab);
    let t = if len2 > 0.0 {
        ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p - (a + ab * t)).length()
}

/// The default join is `Miter`. A 170° turn is past the miter limit, so it bevels and stays
/// about half a stroke-width off the path. `MiterClip` keeps a clipped spike farther out.
#[test]
fn the_default_stroke_join_is_miter() {
    let a = Vec2::ZERO;
    let b = Vec2::new(200.0, 0.0);
    // 170° turn: long edges so the join does not fold, sharp enough to pass the miter limit.
    let turn = 170.0_f32.to_radians();
    let c = b + Vec2::new(turn.cos(), turn.sin()) * 200.0;
    let spike = |clip: Option<bool>| {
        let draw = Draw::new();
        let line = draw.polyline().stroke_weight(4.0);
        let line = match clip {
            Some(true) => line.join_miter_clip(),
            Some(false) => line.join_miter(),
            None => line,
        };
        line.points([a, b, c]);
        build(&draw)
            .0
            .vertices()
            .iter()
            .map(|v| {
                dist_to_segment(Vec2::new(v.position[0], v.position[1]), a, b).min(dist_to_segment(
                    Vec2::new(v.position[0], v.position[1]),
                    b,
                    c,
                ))
            })
            .fold(0.0, f32::max)
    };
    let miter = spike(Some(false));
    let default = spike(None);
    let clip = spike(Some(true));
    assert!(
        (default - miter).abs() < 1e-3,
        "the default join matches Miter (spike {default} vs {miter})"
    );
    assert!(
        clip > miter + 1.0,
        "MiterClip keeps a clipped spike this Miter join bevels away ({clip} vs {miter})"
    );
}

#[test]
fn an_uncolored_line_uses_the_stroke_theme() {
    use regia_quick::draw::theme::{Primitive, Theme};
    let stroke = Theme::default().stroke(&Primitive::Line).to_array();
    assert_ne!(stroke, Color::WHITE.to_array());

    let draw = Draw::new();
    draw.line().points(Vec2::ZERO, Vec2::new(100.0, 0.0));
    let (fast, _) = build(&draw);
    assert!(!fast.vertices().is_empty());
    assert!(fast.vertices().iter().all(|v| v.color == stroke));

    let draw = Draw::new();
    draw.line()
        .points(Vec2::ZERO, Vec2::new(100.0, 0.0))
        .start_cap_round()
        .end_cap_butt();
    let (slow, _) = build(&draw);
    assert!(!slow.vertices().is_empty());
    assert!(slow.vertices().iter().all(|v| v.color == stroke));

    let draw = Draw::new();
    let mut theme = Theme::default();
    theme.stroke_color.primitive.insert(Primitive::Line, Color::RED);
    draw.state
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_theme(theme);
    draw.line().points(Vec2::ZERO, Vec2::new(40.0, 0.0));
    draw.line()
        .points(Vec2::new(0.0, 20.0), Vec2::new(40.0, 20.0))
        .start_cap_round()
        .end_cap_butt();
    let (batch, _) = build(&draw);
    assert!(batch.vertices().iter().all(|v| v.color == Color::RED.to_array()));
}

#[test]
fn a_round_cap_radius_is_half_the_stroke_width_and_a_zero_length_line_is_empty() {
    let draw = Draw::new();
    draw.line()
        .points(Vec2::ZERO, Vec2::new(100.0, 0.0))
        .weight(10.0)
        .caps_round();
    let (batch, _) = build(&draw);
    let max_x = batch
        .vertices()
        .iter()
        .map(|v| v.position[0])
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(
        max_x > 104.0 && max_x < 106.0,
        "radius is half the width (5), not the width; max x = {max_x}"
    );

    for caps in [true, false] {
        let draw = Draw::new();
        let line = draw.line().points(Vec2::ZERO, Vec2::ZERO).weight(10.0);
        if caps {
            line.caps_round();
        } else {
            line.caps_square();
        }
        let (batch, _) = build(&draw);
        assert!(batch.indices().is_empty(), "equal endpoints submit nothing");
    }
}

#[test]
fn ellipse_and_round_caps_honor_stroke_tolerance() {
    let ellipse_vertices = |tolerance: f32| {
        let draw = Draw::new();
        draw.ellipse().radius(200.0).stroke_tolerance(tolerance);
        build(&draw).0.vertices().len()
    };
    assert!(
        ellipse_vertices(8.0) < ellipse_vertices(0.1),
        "a coarser ellipse tolerance uses fewer vertices ({} vs {})",
        ellipse_vertices(8.0),
        ellipse_vertices(0.1)
    );

    let cap_vertices = |tolerance: f32| {
        let draw = Draw::new();
        draw.line()
            .points(Vec2::ZERO, Vec2::new(0.0, 100.0))
            .caps_round()
            .weight(40.0)
            .tolerance(tolerance);
        build(&draw).0.vertices().len()
    };
    assert!(
        cap_vertices(8.0) < cap_vertices(0.1),
        "a coarser round cap uses fewer vertices ({} vs {})",
        cap_vertices(8.0),
        cap_vertices(0.1)
    );
}

#[test]
fn degenerate_lines_draw_nothing() {
    let draw = Draw::new();
    draw.line()
        .start(Vec2::ZERO)
        .end(Vec2::new(f32::NAN, 1.0))
        .weight(2.0);
    draw.line()
        .start(Vec2::ZERO)
        .end(Vec2::new(10.0, 0.0))
        .weight(0.0);
    draw.line()
        .start(Vec2::ZERO)
        .end(Vec2::new(10.0, 0.0))
        .weight(-1.0);
    let (batch, _) = build(&draw);
    assert!(batch.indices().is_empty());
}

/// Finding 1: a filled rect reserves a slot and finishes it. Property setters do not take the
/// shared state write lock.
#[test]
fn perf_rect_builder_takes_two_state_writes() {
    let draw = Draw::new();
    let before = draw.state_write_count();
    draw.rect().x_y(1.0, 2.0).w_h(3.0, 4.0).color(Color::RED);
    let writes = draw.state_write_count() - before;
    assert_eq!(writes, 2, "reserve and finish only, got {writes}");
    let (batch, stats) = build(&draw);
    assert_eq!(batch.vertices()[0].color, Color::RED.to_array());
    assert_eq!(stats.bounds, Some([-0.5, 0.0, 2.5, 4.0]));
    assert_eq!(stats.primitives, 1);
}

/// Finding 2: the same text through one renderer is shaped once. A new string or size shapes again.
#[test]
fn perf_text_is_shaped_once() {
    let draw = Draw::new();
    let mut fonts = TextSystem::embedded_only();
    let localization = Localization::default();
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    draw.text("hello").font_size(32);
    let first = renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(renderer.text_shape_count(), 1);
    assert_eq!(first.texts, vec!["hello".to_string()]);
    let glyphs: Vec<[f32; 2]> = batch
        .vertices()
        .iter()
        .map(|v| [v.position[0], v.position[1]])
        .collect();
    assert!(!glyphs.is_empty());
    let strings = renderer.text_string_allocs();

    draw.text("hello").font_size(32);
    let second = renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(
        renderer.text_shape_count(),
        1,
        "an unchanged string is not shaped again"
    );
    assert_eq!(
        renderer.text_string_allocs(),
        strings,
        "the text buffer is reused"
    );
    assert_eq!(second.texts, vec!["hello".to_string()]);
    let again: Vec<[f32; 2]> = batch
        .vertices()
        .iter()
        .map(|v| [v.position[0], v.position[1]])
        .collect();
    assert_eq!(again, glyphs, "a cache hit replays the same quads");

    draw.text("hello").font_size(16);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(renderer.text_shape_count(), 2, "a new font size shapes again");
    draw.text("hello!").font_size(16);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(renderer.text_shape_count(), 3, "a new string shapes again");
}

/// Finding 3: the second frame reuses both command vectors.
#[test]
fn perf_command_list_is_reused() {
    let draw = Draw::new();
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    for i in 0..8 {
        draw.rect().x(i as f32 * 8.0).w_h(4.0, 4.0);
    }
    let first = renderer.build(&draw, PARAMS, None, &mut batch);
    assert_eq!(first.primitives, 8);
    for i in 0..8 {
        draw.rect().x(i as f32 * 3.0).w_h(2.0, 6.0).color(Color::BLUE);
    }
    let second = renderer.build(&draw, PARAMS, None, &mut batch);
    assert_eq!(
        renderer.last_command_allocs(),
        0,
        "the second build allocated a command vector"
    );
    assert_eq!(second.primitives, 8);
    assert_eq!(second.by_kind.get("rect"), Some(&8));
    assert!(batch.vertices().iter().all(|v| v.color == Color::BLUE.to_array()));
}

/// Finding 4: a default stroke is an 8-vertex ring. A round cap still goes through lyon.
#[test]
fn perf_plain_stroke_skips_lyon() {
    let draw = Draw::new();
    let before = regia_quick::lyon_path_builds();
    draw.rect().w_h(40.0, 20.0).stroke_weight(4.0);
    let (batch, _) = build(&draw);
    let paths = regia_quick::lyon_path_builds() - before;
    assert_eq!(paths, 0, "a plain stroked rect built {paths} lyon paths");
    assert_eq!(batch.vertices().len(), 12, "fill 4 + ring 8");
    // Outer miter corners sit at ±(half + half-width).
    let xs: Vec<f32> = batch.vertices().iter().map(|v| v.position[0]).collect();
    let ys: Vec<f32> = batch.vertices().iter().map(|v| v.position[1]).collect();
    let max_x = xs.iter().copied().fold(0.0_f32, f32::max);
    let max_y = ys.iter().copied().fold(0.0_f32, f32::max);
    assert!(near(max_x, 22.0, 1e-3), "outer x {max_x}");
    assert!(near(max_y, 12.0, 1e-3), "outer y {max_y}");

    let before = regia_quick::lyon_path_builds();
    draw.rect().w_h(40.0, 20.0).no_fill().stroke_weight(4.0);
    let (ring, _) = build(&draw);
    assert_eq!(regia_quick::lyon_path_builds() - before, 0);
    assert_eq!(ring.vertices().len(), 8, "outline only");

    let before = regia_quick::lyon_path_builds();
    draw.arrow()
        .start(Vec2::ZERO)
        .end(Vec2::new(50.0, 0.0))
        .weight(2.0);
    let (arrow, _) = build(&draw);
    assert_eq!(
        regia_quick::lyon_path_builds() - before,
        0,
        "a plain arrow skips lyon"
    );
    assert_eq!(arrow.vertices().len(), 7, "3-vertex head + 4-vertex shaft");

    let before = regia_quick::lyon_path_builds();
    draw.rect().w_h(40.0, 20.0).stroke_weight(4.0).caps_round();
    let (round, _) = build(&draw);
    assert!(
        regia_quick::lyon_path_builds() > before,
        "a round cap still builds a lyon path"
    );
    assert!(round.vertices().len() > 12, "{}", round.vertices().len());
}

/// Finding 5: tessellation writes the batch. The scratch mesh stays empty.
#[test]
fn perf_scratch_mesh_stays_empty() {
    let draw = Draw::new();
    draw.rect().w_h(10.0, 10.0).color(Color::RED);
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    renderer.build(&draw, PARAMS, None, &mut batch);
    assert_eq!(renderer.scratch_vertices(), 0);
    assert_eq!(batch.vertices().len(), 4);
    assert_eq!(batch.vertices()[0].color, Color::RED.to_array());
}

/// Finding 8: the theme is borrowed, and the kind map reuses its strings.
#[test]
fn perf_theme_is_not_cloned() {
    use regia_quick::draw::theme::{Theme, ThemeColor};
    let draw = Draw::new();
    draw.state.write().expect("state").set_theme(Theme {
        fill_color: ThemeColor {
            default: Color::RED,
            primitive: Default::default(),
        },
        stroke_color: ThemeColor {
            default: Color::BLACK,
            primitive: Default::default(),
        },
    });
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    draw.rect().w_h(8.0, 8.0);
    let first = renderer.build(&draw, PARAMS, None, &mut batch);
    assert_eq!(batch.vertices()[0].color, Color::RED.to_array());
    assert_eq!(first.by_kind.get("rect"), Some(&1));
    draw.rect().w_h(8.0, 8.0);
    let second = renderer.build(&draw, PARAMS, None, &mut batch);
    assert_eq!(
        renderer.last_theme_clones(),
        0,
        "the second build cloned the theme"
    );
    assert_eq!(batch.vertices()[0].color, Color::RED.to_array());
    assert_eq!(second.by_kind.get("rect"), Some(&1));
}

/// Every colour in `want` appears on at least one submitted vertex, so a per-point colour
/// that was dropped, defaulted to white or channel-swapped fails.
fn assert_vertex_colors(batch: &ImmediateBatch, want: &[Color]) {
    for c in want {
        assert!(
            batch.vertices().iter().any(|v| v.color == c.to_array()),
            "no vertex carries {c:?}: {:?}",
            batch.vertices().iter().map(|v| v.color).collect::<Vec<_>>()
        );
    }
}

#[test]
fn mesh_points_colored_keeps_a_distinct_color_per_vertex() {
    let draw = Draw::new();
    draw.mesh().points_colored([
        (Vec3::ZERO, Color::RED),
        (Vec3::X * 10.0, Color::GREEN),
        (Vec3::Y * 10.0, Color::BLUE),
    ]);
    let (batch, _) = build(&draw);
    assert_vertex_colors(&batch, &[Color::RED, Color::GREEN, Color::BLUE]);
}

#[test]
fn polygon_points_colored_keeps_a_distinct_color_per_vertex() {
    let draw = Draw::new();
    draw.polygon().points_colored([
        (Vec2::ZERO, Color::RED),
        (Vec2::new(10.0, 0.0), Color::GREEN),
        (Vec2::new(0.0, 10.0), Color::BLUE),
    ]);
    let (batch, _) = build(&draw);
    assert_vertex_colors(&batch, &[Color::RED, Color::GREEN, Color::BLUE]);
}

#[test]
fn polyline_points_colored_keeps_a_distinct_color_per_point() {
    let draw = Draw::new();
    draw.polyline().weight(4.0).points_colored([
        (Vec2::ZERO, Color::RED),
        (Vec2::new(40.0, 0.0), Color::GREEN),
        (Vec2::new(40.0, 40.0), Color::BLUE),
    ]);
    let (batch, _) = build(&draw);
    assert_vertex_colors(&batch, &[Color::RED, Color::GREEN, Color::BLUE]);
}

/// Horizontal extent of the batch's vertices (glyph quads only in these tests).
fn x_extent(batch: &ImmediateBatch) -> f32 {
    let xs = batch.vertices().iter().map(|v| v.position[0]);
    let (lo, hi) = xs.fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), x| {
        (lo.min(x), hi.max(x))
    });
    hi - lo
}

/// The player's text size (`TextSystem::text_scale`) is not part of the per-text stamp. A cached
/// shape must not survive a change of it: the text would keep its old size on screen.
#[test]
fn text_cache_reshapes_when_the_text_size_setting_changes() {
    let draw = Draw::new();
    let mut fonts = TextSystem::embedded_only();
    let localization = Localization::default();
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    draw.text("hello").font_size(32);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    let small = x_extent(&batch);
    assert_eq!(renderer.text_shape_count(), 1);

    fonts.text_scale = 2.0;
    draw.text("hello").font_size(32);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(
        renderer.text_shape_count(),
        2,
        "a text size change must shape the text again"
    );
    let large = x_extent(&batch);
    assert!(
        large > small * 1.5,
        "doubling the text size must widen the text: {small} -> {large}"
    );
}

/// A language switch bumps `Localization::generation`; cached shapes are dropped with it.
#[test]
fn text_cache_reshapes_after_a_language_switch() {
    let draw = Draw::new();
    let mut fonts = TextSystem::embedded_only();
    let mut localization = Localization::default();
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    draw.text("hello").font_size(32);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(renderer.text_shape_count(), 1);
    localization.generation.0 += 1;
    draw.text("hello").font_size(32);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(renderer.text_shape_count(), 2, "a language switch shapes again");
}

/// Cached glyph quads hold atlas rects. Once the atlas recycles a page (LRU, eight pages per
/// kind) those rects may hold other glyphs, so a cache hit would draw the wrong pixels. Every
/// recycle drops the cache.
#[test]
fn text_cache_is_dropped_when_the_glyph_atlas_recycles_a_page() {
    let draw = Draw::new();
    let mut fonts = TextSystem::embedded_only();
    let localization = Localization::default();
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    draw.text("A").font_size(32);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(renderer.text_shape_count(), 1);

    // Large distinct glyphs fill the eight 512 px pages and force a recycle. A handful of
    // texts stays far below the cache's own size limit, so only the recycle can drop "A".
    let printable: String = (b'!'..=b'~').map(char::from).collect();
    let mut texts = 0;
    for size in [240u32, 250, 260, 270] {
        if renderer.glyph_atlas_wipes() > 0 {
            break;
        }
        draw.text(&printable).font_size(size);
        renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
        texts += 1;
    }
    assert!(
        renderer.glyph_atlas_wipes() > 0,
        "precondition: {texts} large texts recycled no atlas page"
    );
    let shapes = renderer.text_shape_count();
    draw.text("A").font_size(32);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(
        renderer.text_shape_count(),
        shapes + 1,
        "after an atlas recycle the cached quads are stale and must not be replayed"
    );
}

/// A drawing still being built has reserved its slot: the draw is not empty.
#[test]
fn draw_with_an_open_drawing_is_not_empty() {
    let draw = Draw::new();
    assert!(draw.is_empty());
    let open = draw.rect().w_h(4.0, 4.0);
    assert!(!draw.is_empty(), "an in-progress drawing counts as drawn");
    drop(open);
    assert!(!draw.is_empty());
    let (_, stats) = build(&draw);
    assert_eq!(stats.primitives, 1);
    assert!(draw.is_empty());
}

/// corr-low (corr-regia-quick-app LOW 5a): open drawings were found by their command index. After
/// a `reset` a new drawing reuses that index, so a drawing begun before the reset edited the new
/// drawing's primitive, and its drop committed the new drawing early, before the rest of its
/// builder calls. A drawing from before the reset must not touch the new one.
#[test]
fn a_drawing_begun_before_a_reset_does_not_edit_the_next_one() {
    let draw = Draw::new();
    let stale = draw.rect();
    draw.reset();
    let fresh = draw.rect();
    // The stale drawing's builder and drop must stay on the stale drawing.
    let stale = stale.w_h(10.0, 10.0);
    drop(stale);
    let fresh = fresh.w_h(2.0, 2.0);
    drop(fresh);
    let (batch, stats) = build(&draw);
    assert_eq!(
        stats.primitives, 1,
        "only the drawing made after the reset is drawn"
    );
    let covered = area(&batch);
    assert!(
        (covered - 4.0).abs() < 1e-3,
        "the fresh rect keeps its own 2x2 size, got area {covered}"
    );
}
