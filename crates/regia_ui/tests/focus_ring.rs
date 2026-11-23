// SPDX-License-Identifier: MPL-2.0
//! Focus ring v2 (lane D9, ADR-372; `docs/specs/ui-default-design.md` §§ 2.2, 3 and 5, gap I05):
//! the ring takes its colour, width and offset from the theme tokens, follows the widget's radius
//! plus the offset, list rows draw it inset so a scroll clip cannot cut it, and it shows in
//! navigation mode only (a pointer click hides it, focus stays). Headless: the draw list is read
//! from the extract pass, no GPU, no goldens, fixed 16 ms steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, MouseButton};
use regia_ui::agent_bridge::host_call;
use regia_ui::focus::{DefaultFocus, FocusRingPolicy, FocusState, UiInputMode, focus_ring_geometry};
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::theme_file::ResolvedTheme;
use regia_ui::theme_resolve::builtin_theme_set;
use regia_ui::{ActiveTheme, Focused, ThemeResolver, UiQuad};
use serde_json::json;

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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

fn theme(name: &str) -> ResolvedTheme {
    builtin_theme_set()
        .resolve(name, None)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn tok(t: &ResolvedTheme, key: &str) -> UiColor {
    t.color(key)
        .unwrap_or_else(|| panic!("no color {key} in {}", t.name))
}

fn num(t: &ResolvedTheme, key: &str) -> f32 {
    t.number(key)
        .unwrap_or_else(|| panic!("no number {key} in {}", t.name))
}

/// A full-window column root with room around its children (no classes: not themed).
fn root(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            UiRoot::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(40.0),
                gap: [Dimension::Points(40.0), Dimension::Points(40.0)],
                ..Default::default()
            }),
        ))
        .id()
}

/// A focusable, clickable widget of `size` UI px carrying `classes`, under `parent`.
fn widget(app: &mut App, parent: Entity, classes: &[&str], size: [f32; 2]) -> Entity {
    let world = app.world_mut();
    let e = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(size[0]), Dimension::Points(size[1])],
                flex_shrink: 0.0,
                ..Default::default()
            }),
            Classes(classes.iter().map(|c| (*c).to_owned()).collect()),
            Focusable::default(),
            Interactable::default(),
        ))
        .id();
    world.entity_mut(parent).add_child(e);
    e
}

fn settle(app: &mut App) {
    for _ in 0..6 {
        app.update();
    }
}

fn key(app: &mut App, logical: &str) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: logical.into(),
            logical_key: logical.into(),
            state,
            repeat: false,
            timestamp: Default::default(),
        });
        app.update();
    }
}

fn click_at(app: &mut App, at: [f32; 2]) {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (f64::from(at[0]), f64::from(at[1])),
        timestamp: Default::default(),
    });
    app.update();
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::MouseButtonInput {
            window: Entity::PLACEHOLDER,
            button: MouseButton::Left,
            state,
            timestamp: Default::default(),
        });
        app.update();
    }
}

/// Focuses `e` the way code does (no input device involved).
fn focus(app: &mut App, e: Entity) {
    app.world_mut().entity_mut(e).insert(Focused);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
}

fn node_rect(app: &App, e: Entity) -> ([f32; 4], f32) {
    let c = app.world().get::<ComputedNode>(e).unwrap();
    (c.rect, c.scale)
}

fn themed(app: &App, e: Entity) -> Themed {
    *app.world().get::<Themed>(e).unwrap()
}

fn quads(app: &App) -> Vec<UiQuad> {
    app.world().resource::<SharedUiRenderer>().lock().quads().to_vec()
}

/// Ring quads: a transparent fill with a `ring`-coloured border band.
fn rings(app: &App, ring: UiColor) -> Vec<UiQuad> {
    quads(app)
        .into_iter()
        .filter(|q| q.color == UiColor::TRANSPARENT && q.color2 == ring && q.border_width[0] > 0.0)
        .collect()
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

fn assert_ring(q: &UiQuad, border_box: [f32; 4], radius: f32, width: f32, offset: f32, what: &str) {
    let grow = offset + width;
    let want = [
        border_box[0] - grow,
        border_box[1] - grow,
        border_box[2] + 2.0 * grow,
        border_box[3] + 2.0 * grow,
    ];
    let got = [q.pos[0], q.pos[1], q.size[0], q.size[1]];
    assert!(
        got.iter().zip(want).all(|(g, w)| close(*g, w)),
        "{what}: ring rect {got:?}, want {want:?}"
    );
    assert!(
        q.border_width.iter().all(|w| close(*w, width)),
        "{what}: ring width {:?}, want {width}",
        q.border_width
    );
    assert!(
        q.corner_radius.iter().all(|r| close(*r, radius + grow)),
        "{what}: outer radius {:?}, want {}",
        q.corner_radius,
        radius + grow
    );
}

#[test]
fn the_ring_uses_the_focus_ring_tokens_and_follows_radius_plus_offset() {
    let mut app = ui_app();
    let r = root(&mut app);
    let button = widget(&mut app, r, &["button"], [200.0, 44.0]);
    settle(&mut app);
    key(&mut app, "ArrowDown");
    settle(&mut app);
    assert_eq!(app.world().resource::<FocusState>().focused, Some(button));

    let dark = theme("regia-dark");
    let ring = tok(&dark, "color.focus-ring");
    assert_ne!(
        ring,
        tok(&dark, "color.accent"),
        "the ring no longer borrows the accent"
    );
    let t = themed(&app, button);
    assert_eq!(t.focus_ring, Some(ring));
    assert_eq!(t.accent, Some(tok(&dark, "color.accent")));
    assert!(t.focus_visible);

    let found = rings(&app, ring);
    assert_eq!(found.len(), 1, "one ring around the focused button");
    let (rect, scale) = node_rect(&app, button);
    assert_ring(
        &found[0],
        rect,
        num(&dark, "radius.md") * scale,
        num(&dark, "border.width.focus") * scale,
        num(&dark, "border.width.focus-offset") * scale,
        "Button",
    );
}

#[test]
fn every_default_widget_type_gets_its_ring_from_its_own_radius() {
    let mut app = ui_app();
    let r = root(&mut app);
    let cases: [(&[&str], [f32; 2], &str); 4] = [
        (&["button", "primary"], [200.0, 44.0], "radius.md"),
        (&["toggle"], [40.0, 24.0], "radius.xl"),
        (&["slider-track"], [240.0, 8.0], "radius.md"),
        (&["panel"], [300.0, 120.0], "radius.md"),
    ];
    let nodes: Vec<Entity> = cases
        .iter()
        .map(|(classes, size, _)| widget(&mut app, r, classes, *size))
        .collect();
    settle(&mut app);
    app.world_mut().insert_resource(UiInputMode::Navigation);
    let dark = theme("regia-dark");
    let ring = tok(&dark, "color.focus-ring");
    for (i, e) in nodes.iter().enumerate() {
        if let Some(prev) = app.world().resource::<FocusState>().focused {
            app.world_mut().entity_mut(prev).remove::<Focused>();
        }
        focus(&mut app, *e);
        settle(&mut app);
        let found = rings(&app, ring);
        let what = cases[i].0.join(".");
        assert_eq!(found.len(), 1, "{what}: one ring");
        let (rect, scale) = node_rect(&app, *e);
        let radius = num(&dark, cases[i].2) * scale;
        let width = num(&dark, "border.width.focus") * scale;
        let offset = num(&dark, "border.width.focus-offset") * scale;
        // The outer radius is clamped to half the ring's short side (a pill stays a pill).
        let grow = offset + width;
        let half = (rect[2] + 2.0 * grow).min(rect[3] + 2.0 * grow) * 0.5;
        assert_ring(
            &found[0],
            rect,
            (radius + grow).min(half) - grow,
            width,
            offset,
            &what,
        );
    }
}

#[test]
fn a_pointer_click_hides_the_ring_and_keeps_focus_until_the_next_navigation_input() {
    let mut app = ui_app();
    let r = root(&mut app);
    let button = widget(&mut app, r, &["button"], [200.0, 44.0]);
    settle(&mut app);
    let ring = tok(&theme("regia-dark"), "color.focus-ring");

    key(&mut app, "ArrowDown");
    settle(&mut app);
    assert_eq!(rings(&app, ring).len(), 1, "keyboard focus shows the ring");

    let (rect, _) = node_rect(&app, button);
    click_at(&mut app, [rect[0] + rect[2] * 0.5, rect[1] + rect[3] * 0.5]);
    settle(&mut app);
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Pointer);
    assert_eq!(
        app.world().resource::<FocusState>().focused,
        Some(button),
        "a click does not move or drop focus"
    );
    let t = themed(&app, button);
    assert!(t.focused, "the node is still focused");
    assert!(!t.focus_visible, "but shows no ring");
    assert!(rings(&app, ring).is_empty(), "no ring after a click");

    // The next keyboard input brings the ring back where focus is.
    key(&mut app, "ArrowDown");
    settle(&mut app);
    assert_eq!(app.world().resource::<FocusState>().focused, Some(button));
    assert_eq!(rings(&app, ring).len(), 1, "navigation shows the ring again");
}

#[test]
fn the_policy_can_always_or_never_show_the_ring() {
    let mut app = ui_app();
    let r = root(&mut app);
    let button = widget(&mut app, r, &["button"], [200.0, 44.0]);
    settle(&mut app);
    let ring = tok(&theme("regia-dark"), "color.focus-ring");
    focus(&mut app, button);
    settle(&mut app);
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Pointer);
    assert!(rings(&app, ring).is_empty(), "default: no ring in pointer mode");

    app.world_mut().insert_resource(FocusRingPolicy::Always);
    settle(&mut app);
    assert_eq!(rings(&app, ring).len(), 1, "Always shows it in pointer mode");

    app.world_mut().insert_resource(FocusRingPolicy::Never);
    app.world_mut().insert_resource(UiInputMode::Navigation);
    settle(&mut app);
    assert!(rings(&app, ring).is_empty(), "Never hides it in navigation mode");
    assert!(themed(&app, button).focused);
}

#[test]
fn list_rows_draw_the_ring_inset_so_the_scroll_clip_keeps_all_of_it() {
    let mut app = ui_app();
    let r = root(&mut app);
    let list = {
        let world = app.world_mut();
        let list = world
            .spawn((
                UiNode,
                ClipChildren,
                NodeStyle::new(Style {
                    size: [Dimension::Points(320.0), Dimension::Points(200.0)],
                    flex_shrink: 0.0,
                    ..Default::default()
                }),
            ))
            .id();
        world.entity_mut(r).add_child(list);
        list
    };
    let row = widget(&mut app, list, &["list-item"], [320.0, 44.0]);
    settle(&mut app);
    app.world_mut().insert_resource(UiInputMode::Navigation);
    focus(&mut app, row);
    settle(&mut app);

    let dark = theme("regia-dark");
    let found = rings(&app, tok(&dark, "color.focus-ring"));
    assert_eq!(found.len(), 1);
    let (rect, scale) = node_rect(&app, row);
    let inset = num(&dark, "border.width.focus-inset");
    assert!(inset < 0.0, "the inset token is negative ({inset})");
    assert_ring(
        &found[0],
        rect,
        num(&dark, "radius.sm") * scale,
        num(&dark, "border.width.focus") * scale,
        inset * scale,
        "List.row",
    );
    // The whole band lies inside the row, so the list's clip (the row's own width) cuts nothing.
    let q = &found[0];
    assert!(q.pos[0] >= rect[0] - 1e-3 && q.pos[1] >= rect[1] - 1e-3);
    assert!(q.pos[0] + q.size[0] <= rect[0] + rect[2] + 1e-3);
    assert!(q.pos[1] + q.size[1] <= rect[1] + rect[3] + 1e-3);
    let clip = q.clip;
    assert!(
        q.pos[0] >= clip[0] - 1e-3 && q.pos[0] + q.size[0] <= clip[0] + clip[2] + 1e-3,
        "ring {:?} inside clip {clip:?}",
        [q.pos, q.size]
    );
}

#[test]
fn high_contrast_draws_a_wider_yellow_ring_and_keeps_rows_inset() {
    let mut app = ui_app();
    let r = root(&mut app);
    let button = widget(&mut app, r, &["button"], [200.0, 44.0]);
    let row = widget(&mut app, r, &["list-item"], [320.0, 44.0]);
    app.world_mut().insert_resource(ActiveTheme {
        high_contrast: true,
        ..ActiveTheme::default()
    });
    app.world_mut().insert_resource(UiInputMode::Navigation);
    settle(&mut app);
    let hc = theme("regia-high-contrast");
    let ring = tok(&hc, "color.focus-ring");
    let width = num(&hc, "border.width.focus");
    assert!(width > num(&theme("regia-dark"), "border.width.focus"));
    for (e, offset_key, what) in [
        (button, "border.width.focus-offset", "Button"),
        (row, "border.width.focus-inset", "List.row"),
    ] {
        focus(&mut app, e);
        settle(&mut app);
        let found = rings(&app, ring);
        assert_eq!(found.len(), 1, "{what}");
        let (rect, scale) = node_rect(&app, e);
        let offset = num(&hc, offset_key);
        if what == "List.row" {
            assert!(offset + width <= 0.0, "the inset row ring stays inside the row");
        }
        let q = &found[0];
        assert!(q.border_width.iter().all(|w| close(*w, width * scale)), "{what}");
        assert!(close(q.pos[0], rect[0] - (offset + width) * scale), "{what}");
        app.world_mut().entity_mut(e).remove::<Focused>();
    }
}

#[test]
fn fills_keep_the_accent_while_the_ring_takes_its_own_colour() {
    let mut app = ui_app();
    let r = root(&mut app);
    let bar = widget(&mut app, r, &["panel"], [200.0, 12.0]);
    app.world_mut().entity_mut(bar).insert(ProgressBar { value: 0.5 });
    settle(&mut app);
    let dark = theme("regia-dark");
    let accent = tok(&dark, "color.accent");
    let (rect, _) = node_rect(&app, bar);
    let fill = quads(&app)
        .into_iter()
        .find(|q| q.color == accent && close(q.size[0], rect[2] * 0.5))
        .expect("progress fill in the accent");
    assert!(close(fill.pos[0], rect[0]));
    assert!(
        quads(&app)
            .iter()
            .all(|q| q.color != tok(&dark, "color.focus-ring")),
        "no fill uses the ring colour"
    );
}

#[test]
fn an_input_mode_flip_re_resolves_only_the_focused_node() {
    let mut app = ui_app();
    let r = root(&mut app);
    let a = widget(&mut app, r, &["button"], [200.0, 44.0]);
    widget(&mut app, r, &["button"], [200.0, 44.0]);
    widget(&mut app, r, &["button"], [200.0, 44.0]);
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    settle(&mut app);
    key(&mut app, "ArrowDown");
    settle(&mut app);
    assert_eq!(app.world().resource::<FocusState>().focused, Some(a));
    assert!(themed(&app, a).focus_visible);

    // A cursor move over empty root padding flips the mode; nothing gets hovered.
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (5.0, 5.0),
        timestamp: Default::default(),
    });
    app.update();
    let stats = app.world().resource::<ThemeResolver>().stats();
    assert_eq!(stats.nodes_resolved, 1, "only the focused node re-resolves");
    assert_eq!(stats.styles_computed, 0, "its style comes from the cache");
    assert!(!themed(&app, a).focus_visible);
    app.update();
    assert_eq!(
        app.world().resource::<ThemeResolver>().stats().nodes_resolved,
        0,
        "the next frame is idle"
    );
}

#[test]
fn the_agent_reads_the_ring_and_ui_focus_reveals_it() {
    let mut app = ui_app();
    let r = root(&mut app);
    let button = widget(&mut app, r, &["button"], [200.0, 44.0]);
    settle(&mut app);
    let state = host_call(app.world_mut(), "ui.state", &json!({})).expect("state");
    assert_eq!(state["focus_ring"]["visible"], false);
    assert_eq!(state["focus_ring"]["policy"], "navigation_only");
    assert!(state["focus_ring"]["rect"].is_null());

    host_call(app.world_mut(), "ui.focus", &json!({ "node": button.to_bits() })).expect("focus");
    settle(&mut app);
    let state = host_call(app.world_mut(), "ui.state", &json!({})).expect("state");
    assert_eq!(
        state["input_mode"], "navigation",
        "ui.focus acts like a Tab press"
    );
    assert_eq!(state["focus_ring"]["visible"], true);
    let dark = theme("regia-dark");
    let (rect, scale) = node_rect(&app, button);
    let grow = (num(&dark, "border.width.focus") + num(&dark, "border.width.focus-offset")) * scale;
    let ring_rect = state["focus_ring"]["rect"].as_array().expect("rect");
    assert_eq!(ring_rect.len(), 4);
    assert!(close(ring_rect[0].as_f64().unwrap() as f32, rect[0] - grow));
    assert!(close(ring_rect[2].as_f64().unwrap() as f32, rect[2] + 2.0 * grow));
    let query = host_call(app.world_mut(), "ui.query", &json!({ "node": button.to_bits() })).expect("query");
    assert_eq!(query["nodes"][0]["focus_visible"], true);

    host_call(app.world_mut(), "ui.click", &json!({ "node": button.to_bits() })).expect("click");
    settle(&mut app);
    let state = host_call(app.world_mut(), "ui.state", &json!({})).expect("state");
    assert_eq!(state["input_mode"], "pointer");
    assert_eq!(state["focus"], button.to_bits());
    assert_eq!(state["focus_ring"]["visible"], false, "a click hides the ring");
    assert!(state["focus_ring"]["rect"].is_null());
}

#[test]
fn geometry_is_shared_with_the_agent_surface() {
    // The same function extract uses: an outer ring and an inset one.
    let outer = focus_ring_geometry([10.0, 10.0, 100.0, 40.0], 8.0, 2.0, 2.0).expect("ring");
    assert_eq!(outer.rect, [6.0, 6.0, 108.0, 48.0]);
    assert_eq!(outer.radius, 12.0);
    let inset = focus_ring_geometry([10.0, 10.0, 100.0, 40.0], 4.0, 2.0, -2.0).expect("ring");
    assert_eq!(inset.rect, [10.0, 10.0, 100.0, 40.0]);
    assert_eq!(inset.radius, 4.0);
}

/// The light high-contrast theme (ADR-1309) widens the ring like the dark one, so it must also take
/// the wider inset (spec § 2.2: `focus-inset` is minus the ring width, high contrast -3); with the
/// inherited -2 a row's ring stuck out 1 px past the row and a scroll clip cut it (ADR-5664).
#[test]
fn light_high_contrast_keeps_list_row_rings_inside_the_row() {
    let mut app = ui_app();
    let r = root(&mut app);
    let row = widget(&mut app, r, &["list-item"], [320.0, 44.0]);
    app.world_mut().insert_resource(ActiveTheme {
        high_contrast: true,
        ..ActiveTheme::named("regia-light")
    });
    app.world_mut().insert_resource(UiInputMode::Navigation);
    settle(&mut app);
    let hc = theme("regia-light-high-contrast");
    let width = num(&hc, "border.width.focus");
    let inset = num(&hc, "border.width.focus-inset");
    assert_eq!(width, 3.0);
    assert_eq!(inset, -width, "the inset is minus the ring width");
    focus(&mut app, row);
    settle(&mut app);
    let found = rings(&app, tok(&hc, "color.focus-ring"));
    assert_eq!(found.len(), 1, "one ring in the light high-contrast colour");
    let (rect, scale) = node_rect(&app, row);
    let q = &found[0];
    assert!(q.border_width.iter().all(|w| close(*w, width * scale)));
    assert!(
        q.pos[0] >= rect[0] - 1e-3 && q.pos[1] >= rect[1] - 1e-3,
        "{q:?} in {rect:?}"
    );
    assert!(
        q.pos[0] + q.size[0] <= rect[0] + rect[2] + 1e-3,
        "{q:?} in {rect:?}"
    );
    assert!(
        q.pos[1] + q.size[1] <= rect[1] + rect[3] + 1e-3,
        "{q:?} in {rect:?}"
    );
}
