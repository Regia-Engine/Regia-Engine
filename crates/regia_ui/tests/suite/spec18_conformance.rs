// SPDX-License-Identifier: MPL-2.0
//! Spec conformance (lane sprint-spec-07-18-runtime-ui, ADR-5615): tests for `docs/specs/18-runtime-ui.md`
//! behavior that was implemented but had no test of its own. `TabBar` (§ 1), `VirtualStick` deflection
//! (§ 1), `UiCanvas` drawing without a layout run (§ 1, ADR-160), the pointer-consumed flag (§ 4 input
//! routing), `Text::line_height` (§ 2) and the Greek, Cyrillic, Hebrew and Devanagari scripts (§ 2).
//! Headless, GPU-free, fixed 100 ms steps, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, MouseButton, TouchPhase};
use regia_ui::layout::UiLayoutStats;
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::widgets::{TabIndex as TabButtonIndex, VirtualAxes};
use regia_ui::{CanvasRect, UiCanvas, UiInputState, UiPointer};

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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app
}

fn computed(rect: [f32; 4], z: f32) -> ComputedNode {
    ComputedNode {
        rect,
        clip_rect: rect,
        scale: 1.0,
        z,
        ..Default::default()
    }
}

fn drain(app: &mut App) -> Vec<UiEvent> {
    app.world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect()
}

fn move_to(app: &mut App, x: f64, y: f64) -> Vec<UiEvent> {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (x, y),
        timestamp: Default::default(),
    });
    app.update();
    drain(app)
}

fn mouse(app: &mut App, state: ButtonState) -> Vec<UiEvent> {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: MouseButton::Left,
        state,
        timestamp: Default::default(),
    });
    app.update();
    drain(app)
}

fn click(app: &mut App, x: f64, y: f64) -> Vec<UiEvent> {
    let mut all = move_to(app, x, y);
    all.extend(mouse(app, ButtonState::Pressed));
    all.extend(mouse(app, ButtonState::Released));
    all
}

fn touch(app: &mut App, id: u64, phase: TouchPhase, x: f64, y: f64) {
    app.world_mut().write_message(regia_platform::TouchInput {
        window: Entity::PLACEHOLDER,
        id,
        phase,
        position: (x, y),
        timestamp: Default::default(),
    });
    app.update();
}

/// The tab buttons the tab bar spawned, in label order.
fn tab_buttons(app: &mut App, bar: Entity) -> Vec<(Entity, usize)> {
    let world = app.world_mut();
    let mut tabs: Vec<(Entity, usize)> = world
        .get::<Children>(bar)
        .map(|children| {
            children
                .iter()
                .filter_map(|child| world.get::<TabButtonIndex>(child).map(|index| (child, index.0)))
                .collect()
        })
        .unwrap_or_default();
    tabs.sort_by_key(|(_, index)| *index);
    tabs
}

/// § 1 `TabBar`: one tab button per label; clicking another tab makes it active and emits
/// `ValueChanged(Index)` on the bar once; clicking the active tab changes nothing.
#[test]
fn a_tab_bar_spawns_one_button_per_label_and_a_click_switches_the_active_tab() {
    let mut app = app();
    let bar = app
        .world_mut()
        .spawn((
            TabBar {
                tabs: vec!["Video".into(), "Audio".into(), "Controls".into()],
                active: 0,
            },
            computed([0.0, 0.0, 300.0, 40.0], 0.0),
        ))
        .id();
    app.update();
    let tabs = tab_buttons(&mut app, bar);
    assert_eq!(
        tabs.iter().map(|(_, index)| *index).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "one tab button per label"
    );
    // Lay the buttons out by hand (the bar has no `UiRoot`, so layout leaves them alone).
    for (i, (tab, _)) in tabs.iter().enumerate() {
        app.world_mut()
            .entity_mut(*tab)
            .insert(computed([i as f32 * 100.0, 0.0, 100.0, 40.0], 1.0));
    }
    app.update();

    let events = click(&mut app, 250.0, 20.0);
    assert_eq!(app.world().get::<TabBar>(bar).unwrap().active, 2);
    let changed: Vec<&UiEvent> = events
        .iter()
        .filter(|e| e.target == bar && matches!(e.kind, UiEventKind::ValueChanged(_)))
        .collect();
    assert_eq!(changed.len(), 1, "{events:?}");
    assert_eq!(changed[0].kind, UiEventKind::ValueChanged(UiValue::Index(2)));

    let again = click(&mut app, 250.0, 20.0);
    assert_eq!(app.world().get::<TabBar>(bar).unwrap().active, 2);
    assert!(
        !again
            .iter()
            .any(|e| e.target == bar && matches!(e.kind, UiEventKind::ValueChanged(_))),
        "clicking the active tab emits nothing: {again:?}"
    );
    // The button labels follow the bar.
    assert_eq!(tab_buttons(&mut app, bar).len(), 3);
}

/// § 1 `VirtualStick`: a finger that lands on the stick drives its deflection (x right, y up, at most
/// length 1) into `VirtualAxes` under its action, and lifting the finger recenters it.
#[test]
fn a_virtual_stick_publishes_its_deflection_under_its_action() {
    let mut app = app();
    let stick = app
        .world_mut()
        .spawn((
            {
                let mut stick = VirtualStick::default();
                stick.action = "Move".into();
                stick.radius = 50.0;
                stick
            },
            Interactable::default(),
            computed([0.0, 0.0, 100.0, 100.0], 0.0),
        ))
        .id();
    let axis = |app: &App| {
        app.world()
            .resource::<VirtualAxes>()
            .0
            .get("Move")
            .copied()
            .unwrap_or([f32::NAN; 2])
    };
    let close = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4;

    touch(&mut app, 7, TouchPhase::Started, 75.0, 50.0);
    app.update();
    assert!(close(axis(&app), [0.5, 0.0]), "half right: {:?}", axis(&app));

    touch(&mut app, 7, TouchPhase::Moved, 50.0, 25.0);
    assert!(close(axis(&app), [0.0, 0.5]), "half up (y up): {:?}", axis(&app));

    touch(&mut app, 7, TouchPhase::Moved, 450.0, 50.0);
    assert!(
        close(axis(&app), [1.0, 0.0]),
        "clamped to length 1: {:?}",
        axis(&app)
    );
    assert!(close(
        app.world().get::<VirtualStick>(stick).unwrap().value,
        [1.0, 0.0]
    ));

    touch(&mut app, 7, TouchPhase::Ended, 450.0, 50.0);
    app.update();
    assert!(close(axis(&app), [0.0, 0.0]), "recentered: {:?}", axis(&app));
}

/// § 4 input routing: the UI marks the pointer consumed while it is over (or pressing) an interactable
/// node, and releases it over empty space, so gameplay can skip pointer input the UI used. The gameplay
/// mirror `UiInputState::pointer_consumed` follows it (it was never written before ADR-5615).
#[test]
fn the_pointer_is_consumed_only_over_interactable_ui() {
    let mut app = app();
    app.world_mut()
        .spawn((Interactable::default(), computed([0.0, 0.0, 100.0, 40.0], 0.0)));
    app.update();
    let consumed = |app: &App| {
        (
            app.world().resource::<UiPointer>().consumed_pointer,
            app.world().resource::<UiInputState>().pointer_consumed,
        )
    };
    move_to(&mut app, 50.0, 20.0);
    assert_eq!(
        consumed(&app),
        (true, true),
        "over a button (pointer flag, gameplay mirror)"
    );
    move_to(&mut app, 400.0, 300.0);
    assert_eq!(consumed(&app), (false, false), "over empty space");
}

/// § 1 `UiCanvas` (ADR-160): rects in the node's unit square draw as quads over the node, and changing
/// them redraws without a layout run.
#[test]
fn canvas_rects_draw_in_the_node_unit_square_without_a_layout_run() {
    let mut app = app();
    let red = UiColor::from_hex("#ff0000").unwrap();
    let green = UiColor::from_hex("#00ff00").unwrap();
    let blue = UiColor::from_hex("#0000ff").unwrap();
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
    let graph = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(200.0), Dimension::Points(100.0)],
                ..Default::default()
            }),
            UiCanvas {
                rects: vec![
                    CanvasRect {
                        min: [0.0, 0.0],
                        max: [0.5, 1.0],
                        color: red,
                    },
                    CanvasRect {
                        min: [0.5, 0.5],
                        max: [1.0, 1.0],
                        color: green,
                    },
                ],
            },
        ))
        .id();
    world.entity_mut(root).add_child(graph);
    for _ in 0..3 {
        app.update();
    }
    let rect = app.world().get::<ComputedNode>(graph).unwrap().rect;
    assert!(rect[2] > 0.0 && rect[3] > 0.0, "laid out: {rect:?}");
    let quad_of = |app: &App, color: UiColor| {
        app.world()
            .resource::<SharedUiRenderer>()
            .lock()
            .quads()
            .iter()
            .find(|q| q.color == color)
            .map(|q| (q.pos, q.size))
    };
    let close = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.01 && (a[1] - b[1]).abs() < 0.01;
    let (pos, size) = quad_of(&app, red).expect("the red bar is drawn");
    assert!(
        close(pos, [rect[0], rect[1]]) && close(size, [rect[2] * 0.5, rect[3]]),
        "{pos:?} {size:?}"
    );
    let (pos, size) = quad_of(&app, green).expect("the green bar is drawn");
    assert!(
        close(pos, [rect[0] + rect[2] * 0.5, rect[1] + rect[3] * 0.5])
            && close(size, [rect[2] * 0.5, rect[3] * 0.5]),
        "{pos:?} {size:?}"
    );

    let before = *app.world().resource::<UiLayoutStats>();
    app.world_mut().get_mut::<UiCanvas>(graph).unwrap().rects[1].color = blue;
    app.update();
    let after = *app.world().resource::<UiLayoutStats>();
    assert!(quad_of(&app, blue).is_some(), "the change is drawn");
    assert!(quad_of(&app, green).is_none(), "the old color is gone");
    assert_eq!(
        (after.full_runs, after.incremental_runs),
        (before.full_runs, before.incremental_runs),
        "a canvas change runs no layout"
    );
}

fn embedded() -> TextSystem {
    TextSystem::embedded_only()
}

/// § 2 `Text::line_height`: the baseline gap is `font_px * line_height`.
#[test]
fn line_height_sets_the_baseline_gap() {
    let mut sys = embedded();
    let loc = Localization::default();
    let gap = |sys: &mut TextSystem, line_height: f32| {
        let mut text = Text::new("one\ntwo").with_style(TextStyle {
            font_px: 20.0,
            ..TextStyle::default()
        });
        text.line_height = line_height;
        let shaped = sys.shape(&text, 1.0, None, &loc);
        assert_eq!(shaped.lines.len(), 2);
        shaped.lines[1].baseline - shaped.lines[0].baseline
    };
    let single = gap(&mut sys, 1.0);
    let double = gap(&mut sys, 2.0);
    assert!((single - 20.0).abs() < 0.5, "{single}");
    assert!((double - 40.0).abs() < 0.5, "{double}");
}

/// § 2 "must render correctly": Greek and Cyrillic shape with the bundled fonts (no `.notdef`), Hebrew
/// runs right to left, and Devanagari conjuncts shape into fewer glyphs than code points.
#[test]
fn greek_cyrillic_hebrew_and_devanagari_shape_with_the_bundled_fonts() {
    let mut sys = embedded();
    let loc = Localization::default();
    for sample in ["Ελληνικά αβγδ ΩΣΦ", "Привет, мир ЖЩЮ"] {
        let shaped = sys.shape(&Text::new(sample), 1.0, None, &loc);
        assert_eq!(shaped.missing_glyphs, 0, "{sample}");
        assert!(!shaped.glyphs.is_empty(), "{sample}");
    }
    let hebrew = sys.shape(&Text::new("שלום עולם"), 1.0, None, &loc);
    assert_eq!(hebrew.missing_glyphs, 0);
    assert!(
        hebrew.lines.first().is_some_and(|line| line.rtl),
        "Hebrew runs right to left"
    );

    let word = "नमस्ते";
    let devanagari = sys.shape(&Text::new(word), 1.0, None, &loc);
    assert_eq!(devanagari.missing_glyphs, 0);
    assert!(
        devanagari.glyphs.len() < word.chars().count(),
        "the स्त conjunct shapes: {} glyphs for {} code points",
        devanagari.glyphs.len(),
        word.chars().count()
    );
}

/// § 2 `TextOverflow::Clip`: an unwrapped line wider than its box draws no glyph ink outside the box, while
/// the same line with the default `Visible` draws past it.
#[test]
fn clip_overflow_keeps_glyphs_inside_the_box_and_visible_does_not() {
    use regia_ui::TextureRef;
    use regia_ui::style::FlexDirection;

    let mut app = app();
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                direction: FlexDirection::Column,
                padding: Edges::px(20.0),
                gap: [Dimension::Points(80.0), Dimension::Points(80.0)],
                ..Default::default()
            }),
        ))
        .id();
    let label = |world: &mut World, overflow: TextOverflow| {
        let mut text = Text::new("Clip this rather long line of text").with_style(TextStyle {
            font_px: 20.0,
            ..TextStyle::default()
        });
        text.wrap = WrapMode::None;
        text.overflow = overflow;
        world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(60.0), Dimension::Points(30.0)],
                    ..Default::default()
                }),
                text,
            ))
            .id()
    };
    let clipped = label(world, TextOverflow::Clip);
    let visible = label(world, TextOverflow::Visible);
    world.entity_mut(root).add_child(clipped);
    world.entity_mut(root).add_child(visible);
    for _ in 0..3 {
        app.update();
    }
    let rect_of = |app: &App, e: Entity| app.world().get::<ComputedNode>(e).unwrap().rect;
    let (clip_rect, visible_rect) = (rect_of(&app, clipped), rect_of(&app, visible));
    assert!(
        visible_rect[1] > clip_rect[1] + clip_rect[3],
        "stacked: {clip_rect:?} {visible_rect:?}"
    );
    // The drawn part of each glyph quad (its bounds cut by its clip), with the row it belongs to.
    let glyphs: Vec<[f32; 4]> = app
        .world()
        .resource::<SharedUiRenderer>()
        .lock()
        .quads()
        .iter()
        .filter(|q| matches!(q.texture, TextureRef::MonoAtlas(_)))
        .map(|q| regia_ui::draw::intersect([q.pos[0], q.pos[1], q.size[0], q.size[1]], q.clip))
        .filter(|ink| ink[2] > 0.0 && ink[3] > 0.0)
        .collect();
    let in_row = |ink: &[f32; 4], row: [f32; 4]| {
        let mid = ink[1] + ink[3] * 0.5;
        mid >= row[1] - 20.0 && mid <= row[1] + row[3] + 20.0
    };
    let right = |row: [f32; 4]| row[0] + row[2];
    let clip_ink: Vec<&[f32; 4]> = glyphs.iter().filter(|ink| in_row(ink, clip_rect)).collect();
    let visible_ink: Vec<&[f32; 4]> = glyphs.iter().filter(|ink| in_row(ink, visible_rect)).collect();
    assert!(
        !clip_ink.is_empty() && !visible_ink.is_empty(),
        "both rows draw glyphs"
    );
    assert!(
        clip_ink
            .iter()
            .all(|ink| ink[0] + ink[2] <= right(clip_rect) + 0.01),
        "Clip draws no ink past the box: {clip_ink:?} vs {clip_rect:?}"
    );
    assert!(
        visible_ink
            .iter()
            .any(|ink| ink[0] + ink[2] > right(visible_rect) + 10.0),
        "Visible draws past the box: {visible_ink:?} vs {visible_rect:?}"
    );
}
