// SPDX-License-Identifier: MPL-2.0
//! `BackdropBlur` extraction (ADR-1836): the component yields a `Backdrop` quad under the node's
//! own background, follows the corner radius, marks damage on change and goes away with the
//! component. No GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::{BackdropBlur, BackgroundKind, Border, TextureRef, UiQuad};

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
    app
}

fn quads(app: &App) -> Vec<UiQuad> {
    app.world().resource::<SharedUiRenderer>().lock().quads().to_vec()
}

fn blur_of(app: &App) -> Option<f32> {
    quads(app)
        .iter()
        .find(|q| q.texture == TextureRef::Backdrop)
        .map(|q| q.blur)
}

fn near(got: Option<f32>, want: f32) -> bool {
    got.is_some_and(|g| (g - want).abs() < 1e-2)
}

fn spawn_panel(app: &mut App, blur: Option<BackdropBlur>) -> Entity {
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(10.0),
                ..Default::default()
            }),
        ))
        .id();
    let mut panel = world.spawn((
        UiNode,
        NodeStyle::new(Style {
            size: [Dimension::Points(120.0), Dimension::Points(60.0)],
            ..Default::default()
        }),
        Background {
            kind: BackgroundKind::Color(UiColor([0.1, 0.2, 0.3, 0.4])),
        },
        Border {
            radius: [9.0; 4],
            ..Default::default()
        },
    ));
    if let Some(blur) = blur {
        panel.insert(blur);
    }
    let panel = panel.id();
    world.entity_mut(root).add_child(panel);
    panel
}

#[test]
fn a_blurred_node_emits_a_backdrop_quad_before_its_tint() {
    let mut app = ui_app();
    spawn_panel(&mut app, Some(BackdropBlur { radius: 6.0 }));
    for _ in 0..4 {
        app.update();
    }
    let all = quads(&app);
    let at = all
        .iter()
        .position(|q| q.texture == TextureRef::Backdrop)
        .expect("a backdrop quad");
    let glass = all[at];
    // Physical px: everything scales with the UI scale of the headless window.
    let scale = glass.size[0] / 120.0;
    assert!(scale > 0.0);
    assert!(
        (glass.blur - 6.0 * scale).abs() < 1e-3,
        "{} at scale {scale}",
        glass.blur
    );
    assert!((glass.corner_radius[0] - 9.0 * scale).abs() < 1e-3);
    assert!((glass.size[1] - 60.0 * scale).abs() < 1e-3);
    // The tint is the next quad, same rect and radius, drawn over the blur.
    let tint = all[at + 1];
    assert_eq!(tint.texture, TextureRef::Rect);
    assert_eq!(
        (tint.pos, tint.size, tint.corner_radius),
        (glass.pos, glass.size, glass.corner_radius)
    );
    assert_eq!(
        all.iter().filter(|q| q.texture == TextureRef::Backdrop).count(),
        1
    );
}

#[test]
fn radius_changes_rebuild_and_removal_drops_the_quad() {
    let mut app = ui_app();
    let panel = spawn_panel(&mut app, None);
    for _ in 0..4 {
        app.update();
    }
    let scale = quads(&app).iter().map(|q| q.size[0]).fold(0.0_f32, f32::max) / 120.0;
    assert!(quads(&app).iter().all(|q| q.texture != TextureRef::Backdrop));

    app.world_mut()
        .entity_mut(panel)
        .insert(BackdropBlur { radius: 4.0 });
    app.update();
    assert!(near(blur_of(&app), 4.0 * scale), "{:?}", blur_of(&app));

    app.world_mut().get_mut::<BackdropBlur>(panel).unwrap().radius = 40.0;
    app.update();
    assert!(near(blur_of(&app), 40.0 * scale), "{:?}", blur_of(&app));

    // Beyond the maximum clamps; zero draws nothing.
    app.world_mut().get_mut::<BackdropBlur>(panel).unwrap().radius = 1000.0;
    app.update();
    assert_eq!(
        quads(&app)
            .iter()
            .find(|q| q.texture == TextureRef::Backdrop)
            .map(|q| q.blur),
        Some(regia_ui::draw::MAX_BACKDROP_SIGMA)
    );
    app.world_mut().get_mut::<BackdropBlur>(panel).unwrap().radius = 0.0;
    app.update();
    assert!(quads(&app).iter().all(|q| q.texture != TextureRef::Backdrop));

    app.world_mut().get_mut::<BackdropBlur>(panel).unwrap().radius = 5.0;
    app.update();
    assert!(quads(&app).iter().any(|q| q.texture == TextureRef::Backdrop));
    app.world_mut().entity_mut(panel).remove::<BackdropBlur>();
    app.update();
    assert!(quads(&app).iter().all(|q| q.texture != TextureRef::Backdrop));
}

/// Closing a frosted panel by despawning it drops its backdrop quad, the next frame and every
/// time (C10, ADR-5420: the open and close cycles of `backdrop_targets.rs`).
#[test]
fn despawning_a_frosted_panel_drops_its_quad_every_cycle() {
    let mut app = ui_app();
    for cycle in 0..5 {
        let panel = spawn_panel(&mut app, Some(BackdropBlur { radius: 6.0 }));
        for _ in 0..2 {
            app.update();
        }
        let open = quads(&app);
        assert_eq!(
            open.iter().filter(|q| q.texture == TextureRef::Backdrop).count(),
            1,
            "cycle {cycle}: open"
        );
        app.world_mut().despawn(panel);
        app.update();
        let closed = quads(&app);
        assert_eq!(
            closed
                .iter()
                .filter(|q| q.texture == TextureRef::Backdrop)
                .count(),
            0,
            "cycle {cycle}: the despawned panel still draws {} quads: {closed:?}",
            closed.len()
        );
    }
}
