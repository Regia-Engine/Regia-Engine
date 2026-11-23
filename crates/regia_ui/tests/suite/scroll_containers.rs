// SPDX-License-Identifier: MPL-2.0
//! Scrolling and clipping containers (ADR-520, UI design series d-12): wheel routing to the view
//! under the pointer, nested views that chain what they cannot use, axis-locked strips, flings from
//! a real pointer drag, presses that stop a fling, rubber-band overscroll, wheel smoothing,
//! scrollbar styles, and a 10,000-row container whose scrolled frames skip the flexbox pass.
//!
//! Headless, no GPU. Game time advances by a fixed 10 ms per update; nothing waits on a clock.
//! Most views carry hand-written `ComputedNode` rects (no `UiRoot`, so layout leaves them alone);
//! the 10,000-row test runs the real layout and extract passes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, MouseButton};
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::style::FlexDirection;
use regia_ui::{
    BackgroundKind, Overscroll, ScrollAxis, ScrollMotion, ScrollVelocity, ScrollbarNodes, ScrollbarStyle,
    Scrollbars, Tween, TweenProperty, TweenValue, UiLayoutStats, UiScales, UiTween, scroll_container,
};

const STEP_MS: u64 = 10;

fn app() -> App {
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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(STEP_MS)));
    app
}

fn computed(rect: [f32; 4], content: [f32; 2], z: f32) -> ComputedNode {
    ComputedNode {
        rect,
        clip_rect: rect,
        content_size: content,
        z,
        scale: 1.0,
        paint_order: 0,
    }
}

/// A hit-testable scroll view with a hand-written layout.
fn view(
    app: &mut App,
    rect: [f32; 4],
    content: [f32; 2],
    z: f32,
    view: ScrollView,
    parent: Option<Entity>,
) -> Entity {
    let mut e = app
        .world_mut()
        .spawn((view, Interactable::default(), computed(rect, content, z)));
    if let Some(parent) = parent {
        e.insert(ChildOf(parent));
    }
    e.id()
}

fn step(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

fn offset(app: &App, e: Entity) -> [f32; 2] {
    app.world().get::<ScrollOffset>(e).unwrap().0
}

fn velocity(app: &App, e: Entity) -> [f32; 2] {
    app.world().get::<ScrollVelocity>(e).unwrap().v
}

fn flings(app: &App, e: Entity) -> u32 {
    app.world().get::<ScrollMotion>(e).unwrap().flings()
}

fn move_to(app: &mut App, x: f64, y: f64) {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (x, y),
        timestamp: Default::default(),
    });
    app.update();
}

fn mouse(app: &mut App, state: ButtonState) {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: MouseButton::Left,
        state,
        timestamp: Default::default(),
    });
    app.update();
}

/// One wheel notch of `dy` pixels (negative scrolls the content up, as a wheel turned toward the user).
fn wheel(app: &mut App, dy: f32) {
    app.world_mut().write_message(regia_platform::MouseWheel {
        window: Entity::PLACEHOLDER,
        delta: (0.0, dy),
        unit: regia_platform::WheelUnit::Pixel,
        timestamp: Default::default(),
    });
    app.update();
}

fn drag(app: &mut App, target: Entity, delta: [f32; 2]) {
    app.world_mut()
        .write_message(UiEvent::new(target, UiEventKind::Drag(delta)));
    app.update();
}

fn drag_end(app: &mut App, target: Entity) {
    app.world_mut()
        .write_message(UiEvent::new(target, UiEventKind::DragEnd));
    app.update();
}

#[test]
fn the_wheel_scrolls_only_the_view_under_the_pointer() {
    let mut app = app();
    let left = view(
        &mut app,
        [0.0, 0.0, 200.0, 300.0],
        [200.0, 1000.0],
        1.0,
        ScrollView::default(),
        None,
    );
    let right = view(
        &mut app,
        [200.0, 0.0, 200.0, 300.0],
        [200.0, 1000.0],
        1.0,
        ScrollView::default(),
        None,
    );
    step(&mut app, 2);
    move_to(&mut app, 100.0, 150.0);
    wheel(&mut app, -40.0);
    assert_eq!(offset(&app, left), [0.0, 40.0]);
    assert_eq!(
        offset(&app, right),
        [0.0, 0.0],
        "before ADR-520 every view scrolled together"
    );
    move_to(&mut app, 300.0, 150.0);
    wheel(&mut app, -40.0);
    assert_eq!(offset(&app, left), [0.0, 40.0]);
    assert_eq!(offset(&app, right), [0.0, 40.0]);
    // A wheel tick does not fling.
    step(&mut app, 10);
    assert_eq!(offset(&app, right), [0.0, 40.0]);
    assert_eq!(velocity(&app, right), [0.0, 0.0]);
}

#[test]
fn a_nested_view_fills_up_first_then_the_rest_chains_outward() {
    let mut app = app();
    let outer = view(
        &mut app,
        [0.0, 0.0, 400.0, 300.0],
        [400.0, 2000.0],
        1.0,
        ScrollView::vertical(),
        None,
    );
    // 400 of content in 150 px: 250 px of range.
    let inner = view(
        &mut app,
        [50.0, 50.0, 300.0, 150.0],
        [300.0, 400.0],
        2.0,
        ScrollView::vertical(),
        Some(outer),
    );
    step(&mut app, 2);
    move_to(&mut app, 200.0, 100.0);
    wheel(&mut app, -100.0);
    assert_eq!((offset(&app, inner)[1], offset(&app, outer)[1]), (100.0, 0.0));
    wheel(&mut app, -100.0);
    assert_eq!((offset(&app, inner)[1], offset(&app, outer)[1]), (200.0, 0.0));
    wheel(&mut app, -100.0);
    assert_eq!(
        (offset(&app, inner)[1], offset(&app, outer)[1]),
        (250.0, 50.0),
        "the inner view takes its last 50 px, the outer view the other 50"
    );
    // Back up: the inner view moves first again.
    wheel(&mut app, 100.0);
    assert_eq!((offset(&app, inner)[1], offset(&app, outer)[1]), (150.0, 50.0));
}

#[test]
fn a_contained_view_never_moves_the_view_around_it() {
    let mut app = app();
    let outer = view(
        &mut app,
        [0.0, 0.0, 400.0, 300.0],
        [400.0, 2000.0],
        1.0,
        ScrollView::vertical(),
        None,
    );
    let inner = view(
        &mut app,
        [50.0, 50.0, 300.0, 150.0],
        [300.0, 400.0],
        2.0,
        ScrollView::vertical().contained(),
        Some(outer),
    );
    step(&mut app, 2);
    move_to(&mut app, 200.0, 100.0);
    for _ in 0..4 {
        wheel(&mut app, -100.0);
    }
    assert_eq!((offset(&app, inner)[1], offset(&app, outer)[1]), (250.0, 0.0));
}

#[test]
fn a_horizontal_strip_in_a_vertical_page_splits_a_diagonal_drag() {
    let mut app = app();
    let page = view(
        &mut app,
        [0.0, 0.0, 400.0, 300.0],
        [400.0, 2000.0],
        1.0,
        ScrollView::vertical(),
        None,
    );
    let strip = view(
        &mut app,
        [0.0, 100.0, 400.0, 100.0],
        [1600.0, 100.0],
        2.0,
        ScrollView::horizontal(),
        Some(page),
    );
    // A card inside the strip (the node the drag started on).
    let card = app
        .world_mut()
        .spawn((
            computed([0.0, 100.0, 120.0, 100.0], [120.0, 100.0], 3.0),
            ChildOf(strip),
        ))
        .id();
    step(&mut app, 1);
    // The pointer moves left 30 and up 40: the content follows it.
    drag(&mut app, card, [-30.0, -40.0]);
    assert_eq!(offset(&app, strip), [30.0, 0.0]);
    assert_eq!(offset(&app, page), [0.0, 40.0]);
}

/// A 300 px square view over 5000 px of content, at the origin, hit-testable.
fn tall_view(app: &mut App, scroll: ScrollView) -> Entity {
    view(app, [0.0, 0.0, 300.0, 300.0], [300.0, 5000.0], 1.0, scroll, None)
}

/// Presses at (150, 200) and drags up 10 px per frame for `moves` frames.
fn press_and_drag_up(app: &mut App, moves: usize) {
    move_to(app, 150.0, 200.0);
    mouse(app, ButtonState::Pressed);
    for i in 1..=moves {
        move_to(app, 150.0, 200.0 - 10.0 * i as f64);
    }
}

#[test]
fn a_released_drag_flings_from_its_recent_speed() {
    let mut app = app();
    let v = tall_view(&mut app, ScrollView::default());
    step(&mut app, 1);
    press_and_drag_up(&mut app, 4);
    assert_eq!(
        offset(&app, v)[1],
        40.0,
        "the content followed the pointer while held"
    );
    assert_eq!(
        velocity(&app, v),
        [0.0, 0.0],
        "no coasting while the finger is down"
    );
    // Release right after the last move: 40 px in 50 ms of gesture is 800 px/s.
    mouse(&mut app, ButtonState::Released);
    assert_eq!(flings(&app, v), 1);
    step(&mut app, 400);
    assert_eq!(velocity(&app, v), [0.0, 0.0]);
    // It coasts v * tau = 800 * 0.18 = 144 px (less the tail under the stop speed).
    let coasted = offset(&app, v)[1] - 40.0;
    assert!((coasted - 144.0).abs() < 2.0, "coasted {coasted}");
}

#[test]
fn a_finger_that_rests_before_lifting_does_not_fling() {
    let mut app = app();
    let v = tall_view(&mut app, ScrollView::default());
    step(&mut app, 1);
    press_and_drag_up(&mut app, 4);
    // 150 ms without movement, still pressed.
    step(&mut app, 15);
    mouse(&mut app, ButtonState::Released);
    step(&mut app, 50);
    assert_eq!(flings(&app, v), 0);
    assert_eq!(offset(&app, v)[1], 40.0);
}

#[test]
fn a_press_stops_a_fling() {
    let mut app = app();
    let v = tall_view(&mut app, ScrollView::default());
    step(&mut app, 1);
    app.world_mut().get_mut::<ScrollVelocity>(v).unwrap().v = [0.0, 3000.0];
    step(&mut app, 5);
    assert!(offset(&app, v)[1] > 0.0, "it was coasting");
    move_to(&mut app, 150.0, 150.0);
    mouse(&mut app, ButtonState::Pressed);
    let held_at = offset(&app, v)[1];
    assert_eq!(velocity(&app, v), [0.0, 0.0]);
    step(&mut app, 5);
    assert_eq!(offset(&app, v)[1], held_at);
    // Releasing without a drag does not start it again.
    mouse(&mut app, ButtonState::Released);
    step(&mut app, 5);
    assert_eq!(offset(&app, v)[1], held_at);
}

#[test]
fn a_bouncing_view_rubber_bands_past_its_edge_and_springs_back() {
    let mut app = app();
    let v = tall_view(
        &mut app,
        ScrollView::default().with_overscroll(Overscroll::Bounce),
    );
    step(&mut app, 1);
    // Pull down from the top: four 50 px pointer moves.
    let mut last = 0.0f32;
    let mut last_gain = f32::MAX;
    for _ in 0..4 {
        drag(&mut app, v, [0.0, 50.0]);
        let now = offset(&app, v)[1];
        let gain = last - now;
        assert!(now < 0.0, "the content shows past its top edge ({now})");
        assert!(gain > 0.0 && gain < 50.0, "the rubber band resists ({gain})");
        assert!(gain < last_gain, "and resists more the further it goes");
        last = now;
        last_gain = gain;
    }
    assert!(last > -200.0);
    drag_end(&mut app, v);
    let mut lowest = last;
    for _ in 0..100 {
        step(&mut app, 1);
        let now = offset(&app, v)[1];
        assert!(
            now <= 0.0,
            "the spring does not overshoot into the content ({now})"
        );
        lowest = lowest.min(now);
    }
    assert_eq!(offset(&app, v)[1], 0.0, "back on the edge");
    assert_eq!(velocity(&app, v), [0.0, 0.0]);
    assert!(lowest <= last);
}

#[test]
fn a_clamping_view_stops_at_its_edge() {
    let mut app = app();
    let v = tall_view(&mut app, ScrollView::default());
    step(&mut app, 1);
    drag(&mut app, v, [0.0, 50.0]);
    assert_eq!(offset(&app, v)[1], 0.0);
    drag_end(&mut app, v);
    // A fling toward the end stops on it.
    app.world_mut().get_mut::<ScrollOffset>(v).unwrap().0[1] = 4650.0;
    app.world_mut().get_mut::<ScrollVelocity>(v).unwrap().v = [0.0, 5000.0];
    step(&mut app, 20);
    assert_eq!(offset(&app, v)[1], 4700.0);
    assert_eq!(velocity(&app, v), [0.0, 0.0]);
}

#[test]
fn wheel_smoothing_eases_and_accumulates() {
    let mut app = app();
    let v = tall_view(&mut app, ScrollView::default().with_wheel_smoothing(0.05));
    step(&mut app, 1);
    move_to(&mut app, 150.0, 150.0);
    wheel(&mut app, -100.0);
    let first = offset(&app, v)[1];
    assert!(first > 0.0 && first < 100.0, "eases instead of jumping ({first})");
    wheel(&mut app, -100.0);
    step(&mut app, 100);
    assert_eq!(offset(&app, v)[1], 200.0, "both notches land");
    assert!(
        app.world()
            .get::<ScrollMotion>(v)
            .unwrap()
            .wheel_target()
            .is_none()
    );
}

// --- Scrollbar styles ------------------------------------------------------------------------

/// A 200 x 300 view over `content`, inside a parent that holds its bars.
fn barred_view(app: &mut App, content: [f32; 2], bars: Scrollbars) -> Entity {
    let parent = app
        .world_mut()
        .spawn(computed([0.0, 0.0, 400.0, 400.0], [400.0, 400.0], 0.0))
        .id();
    app.world_mut()
        .spawn((
            ScrollView::default(),
            computed([0.0, 0.0, 200.0, 300.0], content, 1.0),
            bars,
            ChildOf(parent),
        ))
        .id()
}

fn thumb_of(app: &App, view: Entity) -> Entity {
    app.world()
        .get::<ScrollbarNodes>(view)
        .unwrap()
        .bar(ScrollAxis::Vertical)
        .expect("bar spawned")
        .1
}

fn thumb_alpha(app: &App, thumb: Entity) -> f32 {
    match &app.world().get::<Background>(thumb).unwrap().kind {
        BackgroundKind::Color(c) => c.0[3],
        other => panic!("thumb background {other:?}"),
    }
}

#[test]
fn the_overlay_bar_grows_and_brightens_under_the_pointer() {
    let mut app = app();
    let v = barred_view(&mut app, [200.0, 1500.0], Scrollbars::overlay());
    step(&mut app, 3);
    let thumb = thumb_of(&app, v);
    let style = app.world().get::<NodeStyle>(thumb).unwrap().0.clone();
    assert_eq!(style.size[0], Dimension::Points(10.0));
    assert_eq!(style.inset.left, Dimension::Points(190.0));
    assert_eq!(
        app.world().get::<Border>(thumb).unwrap().radius,
        [5.0; 4],
        "a pill"
    );
    assert_eq!(thumb_alpha(&app, thumb), 0.25);
    // The pointer over the thumb (its layout rect written by hand: no layout runs here).
    app.world_mut()
        .entity_mut(thumb)
        .insert(computed([190.0, 0.0, 10.0, 60.0], [10.0, 60.0], 2.0));
    move_to(&mut app, 195.0, 30.0);
    step(&mut app, 1);
    let style = app.world().get::<NodeStyle>(thumb).unwrap().0.clone();
    assert_eq!(style.size[0], Dimension::Points(14.0));
    assert_eq!(style.inset.left, Dimension::Points(186.0));
    assert_eq!(app.world().get::<Border>(thumb).unwrap().radius, [7.0; 4]);
    assert_eq!(thumb_alpha(&app, thumb), 0.40);
    // Pressed: the active tint.
    mouse(&mut app, ButtonState::Pressed);
    step(&mut app, 1);
    assert_eq!(thumb_alpha(&app, thumb), 0.55);
}

#[test]
fn the_overlay_bar_hides_when_idle() {
    let mut app = app();
    let v = barred_view(&mut app, [200.0, 1500.0], Scrollbars::overlay());
    step(&mut app, 3);
    let thumb = thumb_of(&app, v);
    assert_ne!(
        app.world().get::<NodeStyle>(thumb).unwrap().0.display,
        Display::None
    );
    // 10 ms per update: 120 updates is past the one-second idle limit.
    step(&mut app, 120);
    assert_eq!(
        app.world().get::<NodeStyle>(thumb).unwrap().0.display,
        Display::None
    );
}

#[test]
fn the_hidden_style_draws_no_bar_at_all() {
    let mut app = app();
    let v = barred_view(
        &mut app,
        [200.0, 1500.0],
        Scrollbars::styled(ScrollbarStyle::Hidden),
    );
    step(&mut app, 3);
    let nodes = app.world().get::<ScrollbarNodes>(v).unwrap();
    assert!(nodes.bar(ScrollAxis::Vertical).is_none() && nodes.bar(ScrollAxis::Horizontal).is_none());
    assert!(
        !app.world().get::<ScrollView>(v).unwrap().show_scrollbar,
        "the built-in thumb is off too"
    );
}

// --- 10,000 rows -------------------------------------------------------------------------------

#[test]
fn ten_thousand_rows_scroll_without_a_flexbox_pass() {
    const ROWS: usize = 10_000;
    const ROW_H: f32 = 24.0;
    let mut app = app();
    app.world_mut().resource_mut::<UiScales>().window_size = [800.0, 600.0];
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Points(800.0), Dimension::Points(600.0)],
                ..Default::default()
            }),
        ))
        .id();
    let list = world
        .spawn((
            scroll_container(
                Style {
                    size: [Dimension::Points(400.0), Dimension::Points(600.0)],
                    direction: FlexDirection::Column,
                    ..Default::default()
                },
                ScrollView::vertical(),
            ),
            ChildOf(root),
        ))
        .id();
    let color = UiColor([0.2, 0.3, 0.4, 1.0]);
    let rows: Vec<Entity> = (0..ROWS)
        .map(|_| {
            world
                .spawn((
                    UiNode,
                    NodeStyle::new(Style {
                        size: [Dimension::Points(400.0), Dimension::Points(ROW_H)],
                        flex_shrink: 0.0,
                        ..Default::default()
                    }),
                    Background {
                        kind: BackgroundKind::Color(color),
                    },
                    ChildOf(list),
                ))
                .id()
        })
        .collect();
    step(&mut app, 3);
    let content = app.world().get::<ComputedNode>(list).unwrap().content_size;
    assert_eq!(content[1], ROWS as f32 * ROW_H, "the content is every row");
    move_to(&mut app, 200.0, 300.0);
    step(&mut app, 1);
    let before = *app.world().resource::<UiLayoutStats>();

    const FRAMES: u64 = 20;
    for _ in 0..FRAMES {
        wheel(&mut app, -40.0);
    }
    let after = *app.world().resource::<UiLayoutStats>();
    assert_eq!(offset(&app, list)[1], 800.0);
    assert_eq!(
        after.full_runs, before.full_runs,
        "no full relayout while scrolling"
    );
    assert_eq!(
        after.incremental_runs, before.incremental_runs,
        "no taffy pass either"
    );
    assert_eq!(
        after.scroll_runs,
        before.scroll_runs + FRAMES,
        "one scroll-only walk per frame"
    );
    assert!(after.nodes_walked >= ROWS as u64);
    // Rows moved with the offset, exactly.
    let y = |app: &App, i: usize| app.world().get::<ComputedNode>(rows[i]).unwrap().rect[1];
    assert_eq!(y(&app, 0), -800.0);
    assert_eq!(y(&app, 40), 40.0 * ROW_H - 800.0);
    // Only rows inside the clip reach the draw list: rows 33 to 58 (26 rows) cross the 600 px
    // viewport, plus the built-in thumb.
    let quads = app.world().resource::<SharedUiRenderer>().lock().quads().len();
    assert!((26..=30).contains(&quads), "{quads} quads for 26 visible rows");
    // The scroll container is one child of the root, so a wheel frame used to walk every row
    // to throw the off-screen ones away. The walk stays on the order of the visible rows.
    let walked = app
        .world()
        .resource::<regia_ui::UiHotPathCounts>()
        .extract_nodes_walked;
    assert!(
        walked < 200,
        "extract walked {walked} nodes for about 26 visible rows of {ROWS}"
    );
}

/// One width tween beside a large idle tree stays incremental: no full relayout per sample. (An
/// incremental run moves the persisted layout maps instead of copying them; that has no counter,
/// review rev-kimi-20 finding 3.)
#[test]
fn a_width_tween_stays_incremental_beside_a_large_idle_tree() {
    const IDLE: usize = 1_000;
    let mut app = app();
    app.world_mut().resource_mut::<UiScales>().window_size = [800.0, 600.0];
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Points(800.0), Dimension::Points(600.0)],
                direction: FlexDirection::Column,
                ..Default::default()
            }),
        ))
        .id();
    for _ in 0..IDLE {
        world.spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(40.0), Dimension::Points(8.0)],
                ..Default::default()
            }),
            ChildOf(root),
        ));
    }
    let tweened = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(40.0), Dimension::Points(8.0)],
                ..Default::default()
            }),
            UiTween::new(
                Tween::to(TweenProperty::Width, TweenValue::Number(200.0), 1.0)
                    .from(TweenValue::Number(40.0)),
            ),
            ChildOf(root),
        ))
        .id();
    step(&mut app, 2);
    let before_layout = *app.world().resource::<UiLayoutStats>();
    const FRAMES: u64 = 5;
    step(&mut app, FRAMES as usize);
    assert!(
        app.world().get::<UiTween>(tweened).is_some(),
        "the tween is still running, so each frame writes a width"
    );
    let after_layout = *app.world().resource::<UiLayoutStats>();
    assert_eq!(
        after_layout.full_runs, before_layout.full_runs,
        "a width edit is not a full relayout"
    );
    assert!(
        after_layout.incremental_runs >= before_layout.incremental_runs + FRAMES,
        "each sample is an incremental run ({} -> {})",
        before_layout.incremental_runs,
        after_layout.incremental_runs
    );
}

/// From the TEST Pixel 9 Pro (2026-11-09, ADR-4932): after a drag, the controls screen drew its rows
/// about twice as far up as hit testing, `ui.query` and the screen reader placed them, so a finger
/// on a drawn button pressed whatever lay where the button had been. Layout already moves a scroll
/// view's content by its offset; the draw list must not move it again.
#[test]
fn scrolled_rows_are_drawn_where_hit_testing_finds_them() {
    const ROW_H: f32 = 24.0;
    // No thumb: every quad left is a row.
    let mut bare = ScrollView::vertical();
    bare.show_scrollbar = false;
    let mut app = app();
    app.world_mut().resource_mut::<UiScales>().window_size = [800.0, 600.0];
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Points(800.0), Dimension::Points(600.0)],
                ..Default::default()
            }),
        ))
        .id();
    let list = world
        .spawn((
            scroll_container(
                Style {
                    size: [Dimension::Points(400.0), Dimension::Points(600.0)],
                    direction: FlexDirection::Column,
                    ..Default::default()
                },
                bare,
            ),
            ChildOf(root),
        ))
        .id();
    // Each row a different color, so a quad names its row.
    let rows: Vec<Entity> = (0..100u8)
        .map(|i| {
            world
                .spawn((
                    UiNode,
                    NodeStyle::new(Style {
                        size: [Dimension::Points(400.0), Dimension::Points(ROW_H)],
                        flex_shrink: 0.0,
                        ..Default::default()
                    }),
                    Background {
                        kind: BackgroundKind::Color(UiColor([f32::from(i) / 100.0, 0.5, 0.5, 1.0])),
                    },
                    ChildOf(list),
                ))
                .id()
        })
        .collect();
    step(&mut app, 3);
    move_to(&mut app, 200.0, 300.0);
    step(&mut app, 1);
    for _ in 0..10 {
        wheel(&mut app, -40.0);
    }
    let scrolled = offset(&app, list)[1];
    assert!(scrolled > 100.0, "the view did not scroll: {scrolled}");
    step(&mut app, 1);

    let drawn_y = |app: &App, i: usize| {
        let color = UiColor([i as f32 / 100.0, 0.5, 0.5, 1.0]);
        app.world()
            .resource::<SharedUiRenderer>()
            .lock()
            .quads()
            .iter()
            .find(|q| q.color == color)
            .map(|q| q.pos[1])
    };
    let laid_out_y = |app: &App, i: usize| app.world().get::<ComputedNode>(rows[i]).unwrap().rect[1];
    // Rows in the lower half of the view: on screen whether or not they are drawn twice as far up.
    let lower: Vec<usize> = (0..rows.len())
        .filter(|&i| (300.0..560.0).contains(&laid_out_y(&app, i)))
        .collect();
    assert!(lower.len() >= 5, "{lower:?}");
    for i in lower {
        let laid_out = laid_out_y(&app, i);
        assert!(
            (laid_out - (i as f32 * ROW_H - scrolled)).abs() < 1.0,
            "row {i} laid out at {laid_out}"
        );
        let drawn = drawn_y(&app, i).unwrap_or_else(|| panic!("row {i} not drawn"));
        assert!(
            (drawn - laid_out).abs() < 1.0,
            "row {i} drawn at y {drawn}, but hit testing finds it at {laid_out} (offset {scrolled})"
        );
    }
}
