// SPDX-License-Identifier: MPL-2.0
//! Isolated group opacity, rounded clips and image masks (T11, ADR-2866), and the integer scale
//! mode (Y16): a UI tree goes through layout and extract, its draw list is drawn into an
//! offscreen target on a software adapter (lavapipe), and pixels are read back. Numeric pixel
//! assertions only, no golden images.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_render::RenderDevice;
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::{
    BackgroundKind, GroupOpacity, OffscreenTarget, RoundedClip, ScaleMode, TextureRef, UiLayerBudget, UiMask,
    UiPointer, UiQuad, UiRenderer, UiScales,
};

const W: u32 = 128;
const H: u32 = 128;

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    // Startup picks a default window size; the tests use their own.
    app.update();
    app.insert_resource(UiScales {
        window_size: [W as f32, H as f32],
        scale_factor: 1.0,
    });
    app
}

fn root(app: &mut App, mode: ScaleMode) -> Entity {
    app.world_mut()
        .spawn((
            UiRoot {
                scale_mode: mode,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id()
}

/// An absolutely placed node at (x, y) of size (w, h) inside `parent`.
fn node(app: &mut App, parent: Entity, rect: [f32; 4]) -> Entity {
    let e = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(rect[0]),
                    top: Dimension::Points(rect[1]),
                    right: Dimension::Auto,
                    bottom: Dimension::Auto,
                },
                size: [Dimension::Points(rect[2]), Dimension::Points(rect[3])],
                ..Default::default()
            }),
        ))
        .id();
    app.world_mut().entity_mut(parent).add_child(e);
    e
}

fn fill(app: &mut App, e: Entity, color: [f32; 4]) {
    app.world_mut().entity_mut(e).insert(Background {
        kind: BackgroundKind::Color(UiColor(color)),
    });
}

fn settle(app: &mut App) {
    for _ in 0..3 {
        app.update();
    }
}

fn quads(app: &App) -> Vec<UiQuad> {
    app.world().resource::<SharedUiRenderer>().lock().quads().to_vec()
}

/// Draws `quads` over a target cleared to `clear` (linear RGBA) and returns the RGBA8 pixels.
fn draw(quads: Vec<UiQuad>, clear: [f64; 4], images: &[(u64, u32, u32, Vec<u8>)]) -> Vec<u8> {
    let device = device();
    let target = OffscreenTarget::new(&device, W, H);
    target.clear(&device, clear);
    let mut renderer = UiRenderer::default();
    for (id, w, h, data) in images {
        renderer.add_image_rgba(&device, *id, *w, *h, data);
    }
    renderer.set_quads(quads);
    target.draw_ui(&device, &mut renderer);
    target.read(&device).expect("readback")
}

fn px(img: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2], img[i + 3]]
}

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const BLACK: [f64; 4] = [0.0, 0.0, 0.0, 1.0];
const CLEAR: [f64; 4] = [0.0, 0.0, 0.0, 0.0];

/// A group at (16, 16) holding two opaque red children that overlap in x 48..64, y 32..64.
fn two_children(app: &mut App) -> Entity {
    let r = root(app, ScaleMode::ConstantPixelSize);
    let group = node(app, r, [16.0, 16.0, 96.0, 80.0]);
    let a = node(app, group, [0.0, 0.0, 48.0, 48.0]);
    let b = node(app, group, [32.0, 16.0, 48.0, 48.0]);
    fill(app, a, RED);
    fill(app, b, RED);
    group
}

#[test]
fn isolated_group_overlap_equals_the_non_overlap_pixel() {
    let mut app = ui_app();
    let group = two_children(&mut app);
    app.world_mut().entity_mut(group).insert(GroupOpacity(0.5));
    settle(&mut app);
    let list = quads(&app);
    assert!(
        list.iter().any(|q| q.texture == TextureRef::LayerBegin),
        "a layer was planned"
    );
    let plan = app.world().resource::<SharedUiRenderer>().lock().layer_plan();
    assert_eq!((plan.layers, plan.skipped_single, plan.over_budget), (1, 0, 0));
    let img = draw(list, BLACK, &[]);
    let (only_a, overlap, only_b) = (px(&img, 20, 20), px(&img, 56, 40), px(&img, 76, 70));
    assert_eq!(only_a, overlap, "isolated: one even tint over the overlap");
    assert_eq!(only_b, overlap);
    // Half red over black, and nothing outside the children.
    assert!(
        (100..=230).contains(&overlap[0]) && overlap[1] == 0,
        "{overlap:?}"
    );
    assert_eq!(px(&img, 100, 20), [0, 0, 0, 255]);
}

#[test]
fn per_primitive_opacity_shows_the_overlap() {
    let mut app = ui_app();
    let group = two_children(&mut app);
    // `Opacity` multiplies each primitive: the overlap composites twice.
    app.world_mut().entity_mut(group).insert(Opacity(0.5));
    settle(&mut app);
    let list = quads(&app);
    assert!(list.iter().all(|q| q.texture == TextureRef::Rect));
    let img = draw(list, BLACK, &[]);
    let (only_a, overlap) = (px(&img, 20, 20), px(&img, 56, 40));
    assert!(
        overlap[0] > only_a[0] + 20,
        "non-isolated overlap is redder: {overlap:?} vs {only_a:?}"
    );
}

#[test]
fn the_group_matches_the_non_isolated_draw_where_children_do_not_overlap() {
    // Away from the overlap an isolated group and per-primitive opacity give the same pixel:
    // the blend remap applies once either way.
    let mut isolated = ui_app();
    let g = two_children(&mut isolated);
    isolated.world_mut().entity_mut(g).insert(GroupOpacity(0.5));
    settle(&mut isolated);
    let mut plain = ui_app();
    let g = two_children(&mut plain);
    plain.world_mut().entity_mut(g).insert(Opacity(0.5));
    settle(&mut plain);
    let a = draw(quads(&isolated), BLACK, &[]);
    let b = draw(quads(&plain), BLACK, &[]);
    let (pa, pb) = (px(&a, 20, 20), px(&b, 20, 20));
    for c in 0..3 {
        assert!(pa[c].abs_diff(pb[c]) <= 1, "{pa:?} vs {pb:?}");
    }
}

#[test]
fn opaque_and_single_primitive_groups_skip_the_layer() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let opaque = node(&mut app, r, [0.0, 0.0, 40.0, 40.0]);
    let a = node(&mut app, opaque, [0.0, 0.0, 20.0, 20.0]);
    let b = node(&mut app, opaque, [10.0, 10.0, 20.0, 20.0]);
    fill(&mut app, a, RED);
    fill(&mut app, b, RED);
    app.world_mut().entity_mut(opaque).insert(GroupOpacity(1.0));
    let single = node(&mut app, r, [60.0, 60.0, 40.0, 40.0]);
    let only = node(&mut app, single, [0.0, 0.0, 20.0, 20.0]);
    fill(&mut app, only, RED);
    app.world_mut().entity_mut(single).insert(GroupOpacity(0.5));
    settle(&mut app);
    let plan = app.world().resource::<SharedUiRenderer>().lock().layer_plan();
    assert_eq!(
        (plan.layers, plan.skipped_opaque, plan.skipped_single),
        (0, 1, 1),
        "{plan:?}"
    );
    let list = quads(&app);
    assert!(list.iter().all(|q| q.texture == TextureRef::Rect));
    let folded = list.iter().find(|q| q.pos == [60.0, 60.0]).expect("single child");
    assert_eq!(folded.alpha, 0.5, "the single primitive took the opacity");
}

#[test]
fn over_the_budget_a_group_draws_non_isolated() {
    let mut app = ui_app();
    let group = two_children(&mut app);
    app.world_mut().entity_mut(group).insert(GroupOpacity(0.5));
    app.insert_resource(UiLayerBudget { max_bytes: 1024 });
    settle(&mut app);
    let plan = app.world().resource::<SharedUiRenderer>().lock().layer_plan();
    assert_eq!((plan.layers, plan.over_budget), (0, 1));
    let list = quads(&app);
    assert!(list.iter().all(|q| q.texture == TextureRef::Rect));
    assert!(list.iter().all(|q| q.alpha == 0.5));
    // Raising the budget rebuilds the list with the layer.
    app.insert_resource(UiLayerBudget::default());
    settle(&mut app);
    let plan = app.world().resource::<SharedUiRenderer>().lock().layer_plan();
    assert_eq!((plan.layers, plan.over_budget), (1, 0));
}

#[test]
fn the_layer_pool_reuses_its_texture_after_warm_up() {
    let mut app = ui_app();
    let group = two_children(&mut app);
    app.world_mut().entity_mut(group).insert(GroupOpacity(0.5));
    settle(&mut app);
    let list = quads(&app);
    let device = device();
    let target = OffscreenTarget::new(&device, W, H);
    let mut renderer = UiRenderer::default();
    renderer.set_quads(list.clone());
    target.draw_ui(&device, &mut renderer);
    let warm = renderer.layer_stats();
    assert_eq!((warm.drawn, warm.created_total, warm.pool_slots), (1, 1, 1));
    // Same list, then a list whose layer is a few pixels smaller: same pooled texture.
    let mut smaller = list.clone();
    for q in &mut smaller {
        if matches!(q.texture, TextureRef::LayerBegin | TextureRef::LayerComposite) {
            q.size[0] -= 3.0;
        }
    }
    for i in 0..6 {
        renderer.set_quads(if i % 2 == 0 { list.clone() } else { smaller.clone() });
        target.draw_ui(&device, &mut renderer);
    }
    let after = renderer.layer_stats();
    assert_eq!(
        after.created_total, 1,
        "no layer allocation after warm-up: {after:?}"
    );
    assert_eq!(after.drawn, 1);
    assert!(after.pool_bytes >= 96 * 80 * 4);
}

/// A 64 x 64 node at (32, 32) with corner radius 16 whose child fills it with white.
fn rounded_panel(app: &mut App) -> (Entity, Entity) {
    let r = root(app, ScaleMode::ConstantPixelSize);
    let panel = node(app, r, [32.0, 32.0, 64.0, 64.0]);
    app.world_mut().entity_mut(panel).insert((
        RoundedClip,
        Border {
            color: UiColor::TRANSPARENT,
            widths: [0.0; 4],
            radius: [16.0; 4],
        },
    ));
    let child = node(app, panel, [0.0, 0.0, 64.0, 64.0]);
    fill(app, child, WHITE);
    (panel, child)
}

#[test]
fn a_rounded_clip_has_an_antialiased_edge_and_a_transparent_corner() {
    let mut app = ui_app();
    rounded_panel(&mut app);
    settle(&mut app);
    let list = quads(&app);
    assert!(
        list.iter()
            .any(|q| matches!(q.texture, TextureRef::LayerMask { .. }))
    );
    let img = draw(list, CLEAR, &[]);
    assert_eq!(px(&img, 32, 32)[3], 0, "outside corner is fully transparent");
    assert_eq!(px(&img, 33, 33)[3], 0);
    assert_eq!(px(&img, 64, 32)[3], 255, "top edge middle is inside");
    assert_eq!(px(&img, 42, 42)[3], 255, "inside the arc");
    // Along the corner diagonal the edge pixel is partly covered.
    let diagonal: Vec<u8> = (32..44).map(|i| px(&img, i, i)[3]).collect();
    assert!(
        diagonal.iter().any(|a| *a > 0 && *a < 255),
        "no antialiased edge pixel on the diagonal: {diagonal:?}"
    );
    assert!(diagonal.windows(2).all(|w| w[0] <= w[1]), "{diagonal:?}");
    // The far corner is cut too.
    assert_eq!(px(&img, 95, 95)[3], 0);
    assert_eq!(px(&img, 95, 64)[3], 255);
}

#[test]
fn nested_rounded_clips_intersect() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let outer = node(&mut app, r, [16.0, 16.0, 64.0, 64.0]);
    let inner = node(&mut app, outer, [32.0, 32.0, 64.0, 64.0]);
    for e in [outer, inner] {
        app.world_mut().entity_mut(e).insert((
            RoundedClip,
            Border {
                color: UiColor::TRANSPARENT,
                widths: [0.0; 4],
                radius: [16.0; 4],
            },
        ));
    }
    let content = node(&mut app, inner, [-40.0, -40.0, 200.0, 200.0]);
    fill(&mut app, content, WHITE);
    settle(&mut app);
    let img = draw(quads(&app), CLEAR, &[]);
    // Visible only where both rounded rects cover: outer 16..80, inner 48..112.
    assert_eq!(px(&img, 60, 60)[3], 255, "inside both");
    assert_eq!(px(&img, 48, 48)[3], 0, "inner's cut top-left corner");
    assert_eq!(px(&img, 79, 79)[3], 0, "outer's cut bottom-right corner");
    assert_eq!(px(&img, 90, 60)[3], 0, "outside the outer rect");
    assert_eq!(px(&img, 30, 60)[3], 0, "outside the inner rect");
}

#[test]
fn a_hit_in_the_cut_corner_misses() {
    let mut app = ui_app();
    let (_, child) = rounded_panel(&mut app);
    app.world_mut().entity_mut(child).insert(Interactable::default());
    settle(&mut app);
    let hovered_at = |app: &mut App, p: [f32; 2]| {
        app.world_mut().resource_mut::<UiPointer>().cursor = Some(p);
        app.update();
        app.world().resource::<UiPointer>().hovered
    };
    assert_eq!(hovered_at(&mut app, [64.0, 64.0]), Some(child), "centre hits");
    assert_eq!(hovered_at(&mut app, [33.0, 33.0]), None, "cut corner misses");
    assert_eq!(
        hovered_at(&mut app, [64.0, 33.0]),
        Some(child),
        "edge middle hits"
    );
}

/// A 2 x 1 mask: transparent left texel, opaque right texel.
fn half_mask() -> Vec<u8> {
    vec![255, 255, 255, 0, 255, 255, 255, 255]
}

#[test]
fn a_zero_alpha_mask_region_hides_content_and_is_not_hit() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let panel = node(&mut app, r, [32.0, 32.0, 64.0, 64.0]);
    fill(&mut app, panel, WHITE);
    app.world_mut()
        .entity_mut(panel)
        .insert((UiMask::new(7), Interactable::default()));
    app.world()
        .resource::<SharedUiRenderer>()
        .lock()
        .queue_image_rgba(7, 2, 1, half_mask());
    settle(&mut app);
    let img = draw(quads(&app), BLACK, &[(7, 2, 1, half_mask())]);
    assert_eq!(
        px(&img, 34, 64),
        [0, 0, 0, 255],
        "zero-alpha region hides the panel"
    );
    assert_eq!(px(&img, 94, 64), [255, 255, 255, 255], "opaque region shows it");
    assert_eq!(px(&img, 20, 64), [0, 0, 0, 255], "outside the node");
    app.world_mut().resource_mut::<UiPointer>().cursor = Some([34.0, 64.0]);
    app.update();
    assert_eq!(app.world().resource::<UiPointer>().hovered, None);
    app.world_mut().resource_mut::<UiPointer>().cursor = Some([94.0, 64.0]);
    app.update();
    assert_eq!(app.world().resource::<UiPointer>().hovered, Some(panel));
}

#[test]
fn integer_scale_picks_the_floor_and_letterboxes() {
    let mut app = ui_app();
    app.insert_resource(UiScales {
        window_size: [1000.0, 700.0],
        scale_factor: 1.0,
    });
    let mode = ScaleMode::IntegerScale {
        reference: [320.0, 180.0],
    };
    // 1000 / 320 = 3.125 and 700 / 180 = 3.9: scale 3, layout 960 x 540, centred.
    assert_eq!(mode.scale_for([1000.0, 700.0], 1.0), 3.0);
    assert_eq!(mode.letterbox([1000.0, 700.0], 3.0), [20.0, 80.0, 20.0, 80.0]);
    assert_eq!(mode.scale_for([300.0, 100.0], 1.0), 1.0, "never below 1");
    assert_eq!(mode.scale_for([1920.0, 1080.0], 1.0), 6.0);
    let r = root(&mut app, mode);
    let fill_node = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    app.world_mut().entity_mut(r).add_child(fill_node);
    settle(&mut app);
    let c = *app.world().get::<ComputedNode>(fill_node).expect("laid out");
    assert_eq!(c.scale, 3.0);
    assert_eq!(c.rect, [20.0, 80.0, 960.0, 540.0]);
}

#[test]
fn agent_sets_a_group_and_reports_its_layer() {
    use regia_ui::agent_bridge::call;
    use serde_json::json;
    let mut app = ui_app();
    let group = two_children(&mut app);
    app.world_mut()
        .entity_mut(group)
        .insert(regia_ui::NodeName("Panel".into()));
    settle(&mut app);
    let set = call(
        app.world_mut(),
        "ui.set_group",
        &json!({ "name": "Panel", "group_opacity": 0.5, "clip": "rounded", "mask": { "image": 3, "fit": "contain" } }),
    )
    .expect("set");
    assert_eq!(set["changed"], true);
    assert_eq!(set["after"]["group_opacity"], 0.5);
    assert_eq!(set["after"]["clip"], "rounded");
    assert_eq!(set["after"]["mask"]["fit"], "contain");
    assert_eq!(app.world().get::<GroupOpacity>(group), Some(&GroupOpacity(0.5)));
    // Back to a plain group, then read what it costs.
    call(
        app.world_mut(),
        "ui.set_group",
        &json!({ "node": group.to_bits(), "clip": "none", "mask": null }),
    )
    .expect("unset");
    settle(&mut app);
    let stats = call(app.world_mut(), "ui.layer_stats", &json!({})).expect("stats");
    assert_eq!(stats["nodes"]["groups"], 1);
    assert_eq!(stats["nodes"]["masks"], 0);
    assert_eq!(stats["plan"]["layers"], 1);
    assert_eq!(stats["budget_bytes"], 64 * 1024 * 1024);
    for (params, reason) in [
        (
            json!({ "name": "Panel", "group_opacity": 2 }),
            "ui_group_opacity_out_of_range",
        ),
        (json!({ "name": "Panel", "clip": "oval" }), "ui_clip_unknown"),
        (
            json!({ "name": "Panel", "mask": { "fit": "cover" } }),
            "ui_mask_image_required",
        ),
        (
            json!({ "name": "Panel", "mask": { "image": 1, "fit": "zoom" } }),
            "ui_mask_fit_unknown",
        ),
    ] {
        let err = call(app.world_mut(), "ui.set_group", &params).expect_err("invalid");
        assert_eq!(err.reason, reason);
    }
}

#[test]
fn agent_sets_integer_scale_and_a_dpi_curve() {
    use regia_ui::agent_bridge::call;
    use serde_json::json;
    let mut app = ui_app();
    app.insert_resource(UiScales {
        window_size: [1000.0, 700.0],
        scale_factor: 1.5,
    });
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let out = call(
        app.world_mut(),
        "ui.set_scale_mode",
        &json!({ "mode": "integer", "reference": [320, 180] }),
    )
    .expect("integer");
    assert_eq!(out["roots"][0]["root"], r.to_bits());
    assert_eq!(out["roots"][0]["scale"], 3.0);
    assert_eq!(out["roots"][0]["letterbox"], json!([20.0, 80.0, 20.0, 80.0]));
    // DPI 144 (scale factor 1.5) sits halfway between 96 -> 1.0 and 192 -> 2.0.
    let out = call(
        app.world_mut(),
        "ui.set_scale_mode",
        &json!({ "root": r.to_bits(), "mode": "constant_physical", "dpi_curve": [[96, 1.0], [192, 2.0]] }),
    )
    .expect("curve");
    assert_eq!(out["roots"][0]["scale"], 1.5);
    assert_eq!(out["dpi_curve"], json!([[96.0, 1.0], [192.0, 2.0]]));
    let out = call(
        app.world_mut(),
        "ui.set_scale_mode",
        &json!({ "dpi_curve": [[96, 1.0], [192, 1.25]] }),
    )
    .expect("flatter curve");
    assert_eq!(out["roots"][0]["scale"], 1.125);
    for (params, reason) in [
        (json!({ "mode": "integer" }), "ui_reference_required"),
        (
            json!({ "mode": "integer", "reference": [0, 10] }),
            "ui_reference_invalid",
        ),
        (json!({ "mode": "zoom" }), "ui_scale_mode_unknown"),
        (
            json!({ "dpi_curve": [[192, 1.0], [96, 2.0]] }),
            "ui_dpi_curve_invalid",
        ),
        (json!({ "dpi_curve": [[96]] }), "ui_dpi_curve_invalid"),
        (
            json!({ "root": 12345, "mode": "constant_pixel" }),
            "ui_root_not_found",
        ),
    ] {
        let err = call(app.world_mut(), "ui.set_scale_mode", &params).expect_err("invalid");
        assert_eq!(err.reason, reason, "{params}");
    }
}

#[test]
fn a_dpi_curve_is_piecewise_linear_and_clamped() {
    use regia_ui::DpiCurve;
    let curve = DpiCurve {
        points: vec![[96.0, 1.0], [144.0, 1.25], [192.0, 1.75]],
    };
    assert_eq!(curve.eval(48.0), Some(1.0));
    assert_eq!(curve.eval(120.0), Some(1.125));
    assert_eq!(curve.eval(168.0), Some(1.5));
    assert_eq!(curve.eval(400.0), Some(1.75));
    assert!(DpiCurve { points: vec![] }.validate().is_err());
    assert!(
        DpiCurve {
            points: vec![[96.0, 1.0], [96.0, 2.0]]
        }
        .eval(100.0)
        .is_none()
    );
}

// ---------------------------------------------------------------------------------------------
// Layers stop at the frame.
// ---------------------------------------------------------------------------------------------

fn composite(quads: &[UiQuad]) -> UiQuad {
    *quads
        .iter()
        .find(|q| q.texture == TextureRef::LayerComposite)
        .expect("a layer composite")
}

/// A group whose two children run far past the window on both sides: the layer holds only what the
/// window shows, so a panel sliding offscreen neither wastes layer memory nor falls over the budget.
#[test]
fn a_layer_is_cut_to_the_window() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let group = node(&mut app, r, [-500.0, 16.0, 3000.0, 32.0]);
    let a = node(&mut app, group, [0.0, 0.0, 2000.0, 32.0]);
    let b = node(&mut app, group, [1000.0, 0.0, 2000.0, 32.0]);
    fill(&mut app, a, RED);
    fill(&mut app, b, RED);
    app.world_mut().entity_mut(group).insert(GroupOpacity(0.5));
    // More than the visible part, far less than the 3000 x 32 the subtree covers.
    let visible = u64::from(W) * 32 * 4;
    app.insert_resource(UiLayerBudget { max_bytes: visible });
    settle(&mut app);
    let plan = app.world().resource::<SharedUiRenderer>().lock().layer_plan();
    assert_eq!((plan.layers, plan.over_budget), (1, 0), "{plan:?}");
    assert_eq!(plan.bytes, visible);
    let c = composite(&quads(&app));
    assert_eq!((c.pos, c.size), ([0.0, 16.0], [W as f32, 32.0]));
}

/// A group entirely off the window draws nothing and takes no layer.
#[test]
fn a_group_off_the_window_takes_no_layer() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let group = node(&mut app, r, [1000.0, 16.0, 96.0, 32.0]);
    for x in [0.0, 16.0] {
        let child = node(&mut app, group, [x, 0.0, 32.0, 32.0]);
        fill(&mut app, child, RED);
    }
    app.world_mut().entity_mut(group).insert(GroupOpacity(0.5));
    settle(&mut app);
    let plan = app.world().resource::<SharedUiRenderer>().lock().layer_plan();
    assert_eq!(
        (plan.layers, plan.skipped_empty, plan.bytes),
        (0, 1, 0),
        "{plan:?}"
    );
}

/// Growing the window moves no node (the group is absolutely placed), so layout reports no damage:
/// the layer must still grow, or the newly shown part stays cut off.
#[test]
fn a_window_resize_rebuilds_a_layer_cut_to_the_old_size() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let group = node(&mut app, r, [0.0, 0.0, 400.0, 32.0]);
    for x in [0.0, 200.0] {
        let child = node(&mut app, group, [x, 0.0, 200.0, 32.0]);
        fill(&mut app, child, RED);
    }
    app.world_mut().entity_mut(group).insert(GroupOpacity(0.5));
    settle(&mut app);
    assert_eq!(composite(&quads(&app)).size, [W as f32, 32.0]);
    app.world_mut().resource_mut::<UiScales>().window_size = [300.0, H as f32];
    settle(&mut app);
    assert_eq!(composite(&quads(&app)).size, [300.0, 32.0]);
}

/// With square corners a rounded clip is a rect clip, and the children already have that: no layer.
#[test]
fn a_rounded_clip_with_square_corners_takes_no_layer() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let panel = node(&mut app, r, [16.0, 16.0, 96.0, 96.0]);
    let child = node(&mut app, panel, [-8.0, 0.0, 200.0, 32.0]);
    fill(&mut app, child, RED);
    app.world_mut().entity_mut(panel).insert(RoundedClip);
    settle(&mut app);
    let plan = app.world().resource::<SharedUiRenderer>().lock().layer_plan();
    assert_eq!(plan.layers, 0, "{plan:?}");
    let list = quads(&app);
    assert!(list.iter().all(|q| q.texture == TextureRef::Rect), "{list:?}");
    assert_eq!(
        list[0].clip,
        [16.0, 16.0, 96.0, 96.0],
        "the children keep the rect clip"
    );
}

// ---------------------------------------------------------------------------------------------
// Hit masks agree with what is drawn.
// ---------------------------------------------------------------------------------------------

fn hovered_at(app: &mut App, p: [f32; 2]) -> Option<Entity> {
    app.world_mut().resource_mut::<UiPointer>().cursor = Some(p);
    app.update();
    app.world().resource::<UiPointer>().hovered
}

/// A root draws no group, clip or mask of its own, so it must not cut its children's hits either.
#[test]
fn a_mask_on_a_root_does_not_cut_hits() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let child = node(&mut app, r, [0.0, 0.0, W as f32, H as f32]);
    app.world_mut().entity_mut(child).insert(Interactable::default());
    app.world_mut().entity_mut(r).insert((
        RoundedClip,
        Border {
            color: UiColor::TRANSPARENT,
            widths: [0.0; 4],
            radius: [40.0; 4],
        },
        UiMask::new(99),
    ));
    settle(&mut app);
    assert_eq!(hovered_at(&mut app, [1.0, 1.0]), Some(child));
    assert!(app.world().get::<regia_ui::UiClipMasks>(child).is_none());
}

/// A panel in a window camera's viewport: its masks are in the root's space and the pointer in the
/// window's, so the test has to undo the viewport offset.
#[test]
fn a_camera_viewport_offset_moves_the_hit_mask_with_the_node() {
    let mut app = ui_app();
    let camera = app
        .world_mut()
        .spawn(regia_render::Camera {
            viewport: [0.5, 0.0, 0.5, 1.0],
            ..Default::default()
        })
        .id();
    let r = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..UiRoot::camera(camera)
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    let panel = node(&mut app, r, [8.0, 32.0, 48.0, 48.0]);
    app.world_mut().entity_mut(panel).insert((
        RoundedClip,
        Border {
            color: UiColor::TRANSPARENT,
            widths: [0.0; 4],
            radius: [12.0; 4],
        },
    ));
    let child = node(&mut app, panel, [0.0, 0.0, 48.0, 48.0]);
    app.world_mut().entity_mut(child).insert(Interactable::default());
    settle(&mut app);
    // The viewport starts at x = 64 of the 128 px window; the panel is laid out 1:1 in it.
    assert_eq!(
        app.world().get::<ComputedNode>(panel).map(|c| c.rect),
        Some([8.0, 32.0, 48.0, 48.0])
    );
    assert_eq!(hovered_at(&mut app, [96.0, 56.0]), Some(child), "centre hits");
    assert_eq!(hovered_at(&mut app, [73.0, 33.0]), None, "cut corner misses");
    assert_eq!(
        hovered_at(&mut app, [96.0, 33.0]),
        Some(child),
        "edge middle hits"
    );
}

/// A world panel's hit test (`ui.world_click`) honors the masks as well.
#[test]
fn a_world_panel_hit_honors_the_masks() {
    let mut app = ui_app();
    let r = app
        .world_mut()
        .spawn((
            UiRoot::world_space([128.0, 128.0], 100.0),
            regia_scene::GlobalTransform(regia_core::glam::Affine3A::IDENTITY),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    let panel = node(&mut app, r, [32.0, 32.0, 64.0, 64.0]);
    app.world_mut().entity_mut(panel).insert((
        RoundedClip,
        Border {
            color: UiColor::TRANSPARENT,
            widths: [0.0; 4],
            radius: [16.0; 4],
        },
    ));
    let child = node(&mut app, panel, [0.0, 0.0, 64.0, 64.0]);
    app.world_mut().entity_mut(child).insert(Interactable::default());
    settle(&mut app);
    let world = app.world_mut();
    assert_eq!(
        regia_ui::world_target::hit_test_root(world, r, [64.0, 64.0]),
        Some(child)
    );
    assert_eq!(
        regia_ui::world_target::hit_test_root(world, r, [33.0, 33.0]),
        None
    );
}

/// The masks are only rewritten when something changed: a moved panel must still move them.
#[test]
fn a_moved_panel_moves_its_hit_mask() {
    let mut app = ui_app();
    let (panel, child) = rounded_panel(&mut app);
    app.world_mut().entity_mut(child).insert(Interactable::default());
    settle(&mut app);
    assert_eq!(hovered_at(&mut app, [64.0, 64.0]), Some(child));
    // Settled frames skip the walk; a move after them has to be seen.
    settle(&mut app);
    let moved = NodeStyle::new(Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: Dimension::Points(4.0),
            top: Dimension::Points(4.0),
            right: Dimension::Auto,
            bottom: Dimension::Auto,
        },
        size: [Dimension::Points(64.0), Dimension::Points(64.0)],
        ..Default::default()
    });
    app.world_mut().entity_mut(panel).insert(moved);
    settle(&mut app);
    assert_eq!(hovered_at(&mut app, [36.0, 36.0]), Some(child), "new centre hits");
    assert_eq!(hovered_at(&mut app, [5.0, 5.0]), None, "new cut corner misses");
}

// ---------------------------------------------------------------------------------------------
// Masks named by asset path.
// ---------------------------------------------------------------------------------------------

/// A project with a cooked 2 x 1 RGBA8 image (`half_mask`) and a cooked file of another type. The
/// tests remove `root`.
struct MaskProject {
    root: std::path::PathBuf,
    server: regia_asset::AssetServer,
}

impl MaskProject {
    fn new() -> Self {
        use regia_asset::{
            AssetId, AssetManifest, AssetServer, AssetServerSettings, AssetTypeTag, COOKED_FORMAT_VERSION,
            CookedHeader, CookedImage, ManifestEntry, cooked_cache_dir, cooked_file_name, encode_cooked_file,
        };
        let root = regia_test_utils::unique_temp_dir("ui-mask-asset");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let image = AssetId::new_random();
        let not_an_image = AssetId::new_random();
        let cooked = CookedImage {
            width: 2,
            height: 1,
            rgba8: half_mask(),
        };
        let payload = cooked.to_postcard().expect("payload");
        let file = |id: AssetId, tag, payload: &[u8]| {
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: tag,
                asset_id: id,
                importer_version: 1_000_000,
                dependencies: Vec::new(),
                payload_len: payload.len() as u64,
            };
            encode_cooked_file(&header, payload).expect("encode")
        };
        std::fs::write(
            cache.join(cooked_file_name(image)),
            file(image, AssetTypeTag::image().0, &payload),
        )
        .expect("image");
        std::fs::write(
            cache.join(cooked_file_name(not_an_image)),
            file(not_an_image, AssetTypeTag::heightfield().0, &[]),
        )
        .expect("other");
        let entry = |path: &str, id| ManifestEntry {
            path: path.to_owned(),
            id,
            type_path: "regia_asset::CookedImage".to_owned(),
        };
        AssetManifest {
            entries: vec![
                entry("assets/ui/half.png", image),
                entry("assets/ui/block.png", not_an_image),
            ],
        }
        .write_to_cache(&cache)
        .expect("manifest");
        let server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");
        Self { root, server }
    }
}

#[test]
fn a_mask_named_by_asset_path_hides_content_and_is_not_hit() {
    let project = MaskProject::new();
    let root_dir = project.root.clone();
    let mut app = ui_app();
    app.insert_resource(project.server);
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let panel = node(&mut app, r, [16.0, 16.0, 96.0, 48.0]);
    fill(&mut app, panel, RED);
    app.world_mut().entity_mut(panel).insert((
        Interactable::default(),
        regia_ui::UiMaskAsset::new("assets/ui/half.png"),
    ));
    settle(&mut app);
    // The mask loads in the background (ADR-3148): wait for it, then one update applies it.
    app.world_mut()
        .resource_mut::<regia_ui::UiImageAssets>()
        .finish_loads();
    settle(&mut app);
    let mask = *app.world().get::<UiMask>(panel).expect("resolved to a mask");
    assert_ne!(mask.image & (1 << 63), 0, "out of the ids games pick by hand");
    let renderer = app.world().resource::<SharedUiRenderer>().clone();
    assert_eq!(renderer.lock().image_size(mask.image), Some([2, 1]));
    assert_eq!(hovered_at(&mut app, [32.0, 40.0]), None, "the transparent half");
    assert_eq!(hovered_at(&mut app, [96.0, 40.0]), Some(panel), "the opaque half");
    let img = draw(quads(&app), BLACK, &[(mask.image, 2, 1, half_mask())]);
    assert_eq!(
        px(&img, 24, 40)[0],
        0,
        "the transparent half shows the background"
    );
    assert_eq!(px(&img, 104, 40)[0], 255, "the opaque half shows the panel");
    let _ = std::fs::remove_dir_all(&root_dir);
}

/// A path that does not resolve leaves the node unmasked (with a warning), and never panics.
#[test]
fn a_mask_asset_that_does_not_resolve_leaves_the_node_unmasked() {
    let project = MaskProject::new();
    let root_dir = project.root.clone();
    let mut app = ui_app();
    app.insert_resource(project.server);
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let a = node(&mut app, r, [0.0, 0.0, 8.0, 8.0]);
    let b = node(&mut app, r, [16.0, 0.0, 8.0, 8.0]);
    app.world_mut()
        .entity_mut(a)
        .insert(regia_ui::UiMaskAsset::new("assets/ui/nope.png"));
    app.world_mut()
        .entity_mut(b)
        .insert(regia_ui::UiMaskAsset::new("assets/ui/block.png"));
    settle(&mut app);
    assert!(app.world().get::<UiMask>(a).is_none());
    assert!(app.world().get::<UiMask>(b).is_none());
    let _ = std::fs::remove_dir_all(&root_dir);
}

#[test]
fn agent_sets_a_mask_by_asset_and_reports_bad_paths() {
    use regia_ui::agent_bridge::call;
    use serde_json::json;
    let project = MaskProject::new();
    let root_dir = project.root.clone();
    let mut app = ui_app();
    app.insert_resource(project.server);
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    let panel = node(&mut app, r, [16.0, 16.0, 96.0, 48.0]);
    app.world_mut()
        .entity_mut(panel)
        .insert(regia_ui::NodeName("Panel".into()));
    settle(&mut app);
    let set = call(
        app.world_mut(),
        "ui.set_group",
        &json!({ "name": "Panel", "mask": { "asset": "assets/ui/half.png", "fit": "contain" } }),
    )
    .expect("set");
    assert_eq!(set["after"]["mask"]["asset"], "assets/ui/half.png");
    assert_eq!(set["after"]["mask"]["fit"], "contain");
    let mask = *app.world().get::<UiMask>(panel).expect("mask");
    assert_eq!(mask.fit, regia_ui::MaskFit::Contain);
    assert!(app.world().get::<regia_ui::UiMaskAsset>(panel).is_some());
    // The resolve that follows the insert keeps the same image.
    settle(&mut app);
    assert_eq!(
        app.world().get::<UiMask>(panel).map(|m| m.image),
        Some(mask.image)
    );

    let reason = |app: &mut App, mask: serde_json::Value| {
        let err = call(
            app.world_mut(),
            "ui.set_group",
            &json!({ "name": "Panel", "mask": mask }),
        )
        .expect_err("refused");
        err.reason
    };
    assert_eq!(
        reason(&mut app, json!({ "asset": "assets/ui/nope.png" })),
        "ui_mask_asset_not_found"
    );
    assert_eq!(
        reason(&mut app, json!({ "asset": "assets/ui/block.png" })),
        "ui_mask_asset_failed"
    );
    assert_eq!(
        reason(&mut app, json!({ "asset": "assets/ui/half.png", "image": 3 })),
        "ui_mask_image_required",
        "exactly one of the two"
    );
    // `null` removes both the mask and the name.
    call(
        app.world_mut(),
        "ui.set_group",
        &json!({ "name": "Panel", "mask": null }),
    )
    .expect("remove");
    assert!(app.world().get::<UiMask>(panel).is_none());
    assert!(app.world().get::<regia_ui::UiMaskAsset>(panel).is_none());
    let _ = std::fs::remove_dir_all(&root_dir);
}

#[test]
fn agent_warns_that_a_root_cannot_be_a_group() {
    use regia_ui::agent_bridge::call;
    use serde_json::json;
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    settle(&mut app);
    let set = call(
        app.world_mut(),
        "ui.set_group",
        &json!({ "node": r.to_bits(), "group_opacity": 0.5 }),
    )
    .expect("set");
    assert_eq!(set["warning"], "ui_group_on_root");
    let group = node(&mut app, r, [0.0, 0.0, 8.0, 8.0]);
    settle(&mut app);
    let set = call(
        app.world_mut(),
        "ui.set_group",
        &json!({ "node": group.to_bits(), "group_opacity": 0.5 }),
    )
    .expect("set");
    assert!(set.get("warning").is_none());
}

// ---------------------------------------------------------------------------------------------
// The project's UI settings file.
// ---------------------------------------------------------------------------------------------

fn settings_app(text: &str) -> (App, std::path::PathBuf) {
    use regia_platform::{DirMount, MountPoint, Vfs};
    let dir = regia_test_utils::unique_temp_dir("ui-settings");
    std::fs::create_dir_all(dir.join("ui")).expect("ui dir");
    std::fs::write(dir.join("ui/project.uisettings.ron"), text).expect("file");
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::Assets, DirMount::read_only(dir.clone()));
    let mut app = App::new();
    app.insert_resource(vfs);
    (app, dir)
}

const SETTINGS: &str = "(version: 1, dpi_curve: [(96.0, 1.0), (192.0, 2.0)], layer_budget_bytes: 1024)";

#[test]
fn the_project_settings_file_sets_the_curve_and_the_budget() {
    let (mut app, dir) = settings_app(SETTINGS);
    regia_ui::scale::load_ui_settings(app.world_mut());
    let settings = app.world().resource::<regia_ui::UiScaleSettings>();
    assert_eq!(
        settings.dpi_curve.as_ref().map(|c| c.points.clone()),
        Some(vec![[96.0, 1.0], [192.0, 2.0]])
    );
    assert_eq!(app.world().resource::<UiLayerBudget>().max_bytes, 1024);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn code_wins_over_the_project_settings_file() {
    let (mut app, dir) = settings_app(SETTINGS);
    let curve = regia_ui::DpiCurve {
        points: vec![[96.0, 3.0]],
    };
    app.insert_resource(regia_ui::UiScaleSettings {
        dpi_curve: Some(curve.clone()),
        ..Default::default()
    });
    app.insert_resource(UiLayerBudget { max_bytes: 2048 });
    regia_ui::scale::load_ui_settings(app.world_mut());
    assert_eq!(
        app.world().resource::<regia_ui::UiScaleSettings>().dpi_curve,
        Some(curve)
    );
    assert_eq!(app.world().resource::<UiLayerBudget>().max_bytes, 2048);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A bad file changes nothing and does not stop the game; no file at all is the normal case.
#[test]
fn a_bad_or_missing_settings_file_changes_nothing() {
    for bad in [
        "(version: 3)",
        "(version: 1, unknown_field: 1)",
        "(version: 1, dpi_curve: [(192.0, 1.0), (96.0, 2.0)])",
        "not ron",
    ] {
        let (mut app, dir) = settings_app(bad);
        regia_ui::scale::load_ui_settings(app.world_mut());
        assert!(
            !app.world().contains_resource::<regia_ui::UiScaleSettings>(),
            "{bad}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    let mut app = App::new();
    app.insert_resource(regia_platform::Vfs::new());
    regia_ui::scale::load_ui_settings(app.world_mut());
    assert!(!app.world().contains_resource::<regia_ui::UiScaleSettings>());
}

#[test]
fn the_settings_file_parser_reports_what_is_wrong() {
    use regia_ui::{UI_SETTINGS_VERSION, UiError, parse_ui_settings_ron};
    let file = parse_ui_settings_ron(include_str!("fixtures/ui.example.uisettings.ron"), "fixture")
        .expect("fixture");
    assert_eq!(file.version, UI_SETTINGS_VERSION);
    assert_eq!(file.layer_budget_bytes, Some(32 << 20));
    let current = parse_ui_settings_ron("(version: 2)", "x").expect("empty v2");
    assert_eq!(current.version, UI_SETTINGS_VERSION);
    assert!(current.scale_mode.is_none());
    assert!(matches!(
        parse_ui_settings_ron("(version: 3)", "x"),
        Err(UiError::UnsupportedVersion { found: 3, .. })
    ));
    let curve = parse_ui_settings_ron("(version: 1, dpi_curve: [])", "x");
    assert!(matches!(curve, Err(UiError::Invalid { .. })), "{curve:?}");
}

/// Spawns the root children of a `.ui.ron` document under `parent` through the loader's path.
fn spawn_ui_ron(app: &mut App, parent: Entity, source: &str) -> Vec<Entity> {
    let file = regia_ui::parse_ui_ron(source, "test.ui.ron", &regia_ui::UiMigrators::default())
        .unwrap_or_else(|e| panic!("parse: {e}"));
    file.root
        .children
        .iter()
        .map(|def| regia_ui::authoring::spawn_node(app.world_mut(), parent, def))
        .collect()
}

/// The image quad drawn with texture `id`.
fn image_quad(quads: &[UiQuad], id: u64) -> UiQuad {
    quads
        .iter()
        .copied()
        .find(|q| q.texture == TextureRef::Image(id))
        .unwrap_or_else(|| panic!("no quad of image {id}"))
}

/// `.ui.ron` v11 image `fit` (ADR-9980): a 2:1 picture in a 1:1 node fills the node and samples
/// the centre half of its UVs with `cover`, is letterboxed with `contain` (and with v10's
/// `preserve_aspect: true`), and fills the node unscaled with `stretch`. CPU only: the draw list.
#[test]
fn authoring_image_fit_cover_samples_the_centre_half() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    {
        let renderer = app.world().resource::<SharedUiRenderer>();
        let mut renderer = renderer.lock();
        for id in [21, 22, 23, 24] {
            renderer.queue_image_rgba(id, 2, 1, half_mask());
        }
    }
    spawn_ui_ron(
        &mut app,
        r,
        r#"(version: 11, root: (children: [
            (style: (position: absolute, inset: (0, 0, Auto, Auto), size: (64, 64)), image: (id: 21, fit: cover)),
            (style: (position: absolute, inset: (64, 0, Auto, Auto), size: (64, 64)), image: (id: 22, fit: contain)),
            (style: (position: absolute, inset: (0, 64, Auto, Auto), size: (64, 64)), image: (id: 23, fit: stretch)),
            (style: (position: absolute, inset: (64, 64, Auto, Auto), size: (64, 64)), image: (id: 24, preserve_aspect: true)),
        ]))"#,
    );
    settle(&mut app);
    let quads = quads(&app);
    let close = |a: [f32; 4], b: [f32; 4]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4);

    let cover = image_quad(&quads, 21);
    assert!(
        close(cover.uv, [0.25, 0.0, 0.75, 1.0]),
        "cover crops: {:?}",
        cover.uv
    );
    assert_eq!(
        (cover.pos, cover.size),
        ([0.0, 0.0], [64.0, 64.0]),
        "cover fills the node"
    );

    let contain = image_quad(&quads, 22);
    assert!(close(contain.uv, [0.0, 0.0, 1.0, 1.0]), "{:?}", contain.uv);
    assert_eq!(
        (contain.pos, contain.size),
        ([64.0, 16.0], [64.0, 32.0]),
        "contain letterboxes"
    );

    let stretch = image_quad(&quads, 23);
    assert!(close(stretch.uv, [0.0, 0.0, 1.0, 1.0]), "{:?}", stretch.uv);
    assert_eq!(
        (stretch.pos, stretch.size),
        ([0.0, 64.0], [64.0, 64.0]),
        "stretch fills"
    );

    let kept = image_quad(&quads, 24);
    assert_eq!(
        (kept.pos, kept.size, kept.uv),
        ([64.0, 80.0], [64.0, 32.0], [0.0, 0.0, 1.0, 1.0]),
        "v10's preserve_aspect reads as contain"
    );
}

/// `.ui.ron` v11 fold (ADR-7751) image `fit: tile`: a 16 x 8 picture in a 40 x 20 node repeats at
/// its native size from the node's top-left corner (3 x 3 copies, each 16 x 8 at scale 1, full
/// UVs), and every copy is clipped to the node's rect, so the right and bottom copies show only
/// their first 8 and 4 px. CPU only: the draw list.
#[test]
fn authoring_image_fit_tile_repeats_at_native_size_clipped_to_the_rect() {
    let mut app = ui_app();
    let r = root(&mut app, ScaleMode::ConstantPixelSize);
    {
        let renderer = app.world().resource::<SharedUiRenderer>();
        renderer.lock().queue_image_rgba(31, 16, 8, vec![255; 16 * 8 * 4]);
    }
    spawn_ui_ron(
        &mut app,
        r,
        r#"(version: 11, root: (children: [
            (style: (position: absolute, inset: (10, 20, Auto, Auto), size: (40, 20)), image: (id: 31, fit: tile)),
        ]))"#,
    );
    settle(&mut app);
    let tiles: Vec<UiQuad> = quads(&app)
        .into_iter()
        .filter(|q| q.texture == TextureRef::Image(31))
        .collect();
    assert_eq!(tiles.len(), 9, "3 x 3 copies: {tiles:?}");
    for (i, tile) in tiles.iter().enumerate() {
        let (col, row) = ((i % 3) as f32, (i / 3) as f32);
        assert_eq!(tile.pos, [10.0 + 16.0 * col, 20.0 + 8.0 * row], "copy {i}");
        assert_eq!(tile.size, [16.0, 8.0], "native size, copy {i}");
        assert_eq!(tile.uv, [0.0, 0.0, 1.0, 1.0], "whole picture, copy {i}");
        assert_eq!(tile.clip, [10.0, 20.0, 40.0, 20.0], "cut to the node, copy {i}");
    }
}
