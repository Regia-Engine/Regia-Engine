// SPDX-License-Identifier: MPL-2.0
//! Layout semantics tests: how the taffy-backed tree positions children (`specs/18` § 1).

use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::style::{Align, FlexDirection};

/// Builds an app with `UiPlugin` and the platform messages its systems read.
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

/// A root + column page like the gallery's, with one `labeled_row`-shaped child.
fn gallery_like_row(world: &mut World) -> (Entity, Entity, Entity, Entity, Entity) {
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(16.0),
                gap: [Dimension::Points(12.0), Dimension::Points(12.0)],
                align_items: Align::Stretch,
                ..Default::default()
            }),
        ))
        .id();
    let page = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                padding: Edges::px(12.0),
                flex_grow: 1.0,
                align_items: Align::Stretch,
                ..Default::default()
            }),
        ))
        .id();
    world.entity_mut(root).add_child(page);
    let row = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(4.0), Dimension::Points(4.0)],
                align_items: Align::Stretch,
                ..Default::default()
            }),
        ))
        .id();
    world.entity_mut(page).add_child(row);
    let caption = world
        .spawn((
            UiNode,
            Text::new("Buttons").with_style(TextStyle {
                font_px: 14.0,
                ..Default::default()
            }),
        ))
        .id();
    world.entity_mut(row).add_child(caption);
    let toggle = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(64.0), Dimension::Points(32.0)],
                ..Default::default()
            }),
            Toggle { checked: true },
        ))
        .id();
    world.entity_mut(row).add_child(toggle);
    (root, page, row, caption, toggle)
}

#[test]
fn stretch_rows_stretch_text_and_pack_children_to_start() {
    let mut app = ui_app();
    app.update();
    let (root, page, row, caption, toggle) = gallery_like_row(app.world_mut());
    app.update();
    app.update(); // wrapped-text refinement settles

    let world = app.world();
    let root = world.get::<ComputedNode>(root).expect("root laid out");
    let page = world.get::<ComputedNode>(page).expect("page laid out");
    let row = world.get::<ComputedNode>(row).expect("row laid out");
    let caption = world.get::<ComputedNode>(caption).expect("caption laid out");
    let toggle = world.get::<ComputedNode>(toggle).expect("toggle laid out");

    // The page fills the window (minus the root's 16px side padding) and packs its rows
    // from the top (justify Start).
    assert!(
        (page.rect[2] - (root.rect[2] - 32.0)).abs() < 1.0,
        "page should fill the window width minus root padding"
    );
    assert!(
        row.rect[1] <= page.rect[1] + 40.0,
        "row should pack to the page top, got y={}",
        row.rect[1]
    );

    // `align_items: Stretch` stretches the measured caption leaf to the row width...
    assert!(
        (caption.rect[2] - row.rect[2]).abs() < 1.0,
        "caption should stretch to the row width {} (Auto margins must not shrink+center it); got {}",
        row.rect[2],
        caption.rect[2]
    );
    assert!(
        (caption.rect[0] - row.rect[0]).abs() < 0.5,
        "caption should start at the row's left edge"
    );
    // ...and keeps a definite-size child at the start, not centred.
    assert!(
        (toggle.rect[0] - row.rect[0]).abs() < 0.5,
        "fixed-size child should sit at the row's left edge, got x={}",
        toggle.rect[0]
    );
    assert!(
        (toggle.rect[2] - 64.0).abs() < 0.6,
        "fixed-size child keeps its size (got {})",
        toggle.rect[2]
    );
}

#[test]
fn paint_order_is_the_tree_preorder_so_later_siblings_win_hit_ties() {
    let mut app = ui_app();
    app.update();
    let (root, page, row, caption, toggle) = gallery_like_row(app.world_mut());
    app.update();
    let order = |e: Entity| app.world().get::<ComputedNode>(e).expect("laid out").paint_order;
    assert!(order(root) < order(page));
    assert!(order(page) < order(row));
    assert!(order(row) < order(caption));
    assert!(
        order(caption) < order(toggle),
        "later sibling paints (and hits) above"
    );
}
