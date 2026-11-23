// SPDX-License-Identifier: MPL-2.0
//! Damage-tracked redraw (lane ui-look): a steady UI keeps its draw list (no re-extract, no
//! instance re-upload), and a visual change rebuilds it exactly once.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::style::FlexDirection;
use regia_ui::{BackgroundKind, UiDamage, UiDamageSet, UiQuad};

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

fn generation(app: &App) -> u64 {
    app.world()
        .resource::<SharedUiRenderer>()
        .lock()
        .quads_generation()
}

#[test]
fn steady_ui_is_not_re_extracted() {
    let mut app = ui_app();
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(16.0),
                ..Default::default()
            }),
        ))
        .id();
    let panel = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(200.0), Dimension::Auto],
                padding: Edges::px(8.0),
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor::from_hex("#0c1117").unwrap()),
            },
            Shadow::default(),
        ))
        .id();
    let label = world.spawn((UiNode, Text::new("Supply 2,450"))).id();
    world.entity_mut(root).add_child(panel);
    world.entity_mut(panel).add_child(label);

    for _ in 0..4 {
        app.update();
    }
    let settled = generation(&app);
    let quads = app.world().resource::<SharedUiRenderer>().lock().quads().len();
    assert!(quads > 3, "panel, shadow and glyphs extracted ({quads})");
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(
        generation(&app),
        settled,
        "steady frames must not rebuild the draw list"
    );
    assert!(!app.world().resource::<UiDamage>().0);
    assert_eq!(
        app.world().resource::<SharedUiRenderer>().lock().quads().len(),
        quads,
        "the kept draw list still holds the UI"
    );

    // A colour change rebuilds once, then the UI is steady again.
    app.world_mut().get_mut::<Background>(panel).unwrap().kind = BackgroundKind::Color(UiColor::WHITE);
    app.update();
    assert_eq!(generation(&app), settled + 1);
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(generation(&app), settled + 1);

    // New text content: re-shaped, re-extracted.
    *app.world_mut().get_mut::<Text>(label).unwrap() = Text::new("Supply 2,451");
    app.update();
    app.update();
    assert!(generation(&app) > settled + 1);

    // Despawning the panel removes it from the draw list.
    app.world_mut().entity_mut(panel).despawn();
    app.update();
    assert!(
        app.world()
            .resource::<SharedUiRenderer>()
            .lock()
            .quads()
            .is_empty()
    );
}

#[test]
fn partial_rebuild_matches_full_rebuild() {
    let mut app = ui_app();
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                direction: FlexDirection::Column,
                ..Default::default()
            }),
        ))
        .id();
    let mut bars = Vec::new();
    for i in 0..3 {
        let panel = world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(220.0), Dimension::Points(40.0)],
                    direction: FlexDirection::Column,
                    ..Default::default()
                }),
                Background {
                    kind: BackgroundKind::Color(UiColor::from_hex("#0c1117").unwrap()),
                },
            ))
            .id();
        let label = world.spawn((UiNode, Text::new(format!("Panel {i}")))).id();
        let bar = world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Percent(40.0), Dimension::Points(4.0)],
                    ..Default::default()
                }),
                Background {
                    kind: BackgroundKind::Color(UiColor::WHITE),
                },
            ))
            .id();
        world.entity_mut(root).add_child(panel);
        world.entity_mut(panel).add_children(&[label, bar]);
        bars.push(bar);
    }
    for _ in 0..3 {
        app.update();
    }
    // Animate the middle panel's bar: only its subtree is re-walked.
    for step in 0..5 {
        app.world_mut().get_mut::<NodeStyle>(bars[1]).unwrap().0.size[0] =
            Dimension::Percent(40.0 + 10.0 * step as f32);
        app.update();
        let partial: Vec<UiQuad> = app.world().resource::<SharedUiRenderer>().lock().quads().to_vec();
        {
            let world = app.world_mut();
            world.resource_mut::<UiDamageSet>().full = true;
            world.resource_mut::<UiDamage>().0 = true;
            regia_ui::extract::extract_ui(world);
        }
        let full: Vec<UiQuad> = app.world().resource::<SharedUiRenderer>().lock().quads().to_vec();
        assert_eq!(
            partial, full,
            "step {step}: subtree-cached draw list differs from a full rebuild"
        );
        let bar_quad = full
            .iter()
            .find(|q| q.color == UiColor::WHITE && q.size[1] == 4.0 && q.pos[1] > 40.0 && q.pos[1] < 80.0)
            .expect("middle bar quad");
        assert!((bar_quad.size[0] - 220.0 * (0.4 + 0.1 * step as f32)).abs() < 1.0);
    }
}
