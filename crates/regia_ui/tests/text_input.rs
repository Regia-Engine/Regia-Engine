// SPDX-License-Identifier: MPL-2.0
//! TextInput completion, part 1 (ADR-273 W01, ADR-336): keys, clipboard, pointer selection on
//! shaped text, scrolling and IME commits, through the same platform messages a window sends.
//! Headless: no window, no GPU, the embedded fonts only, game time advances 100 ms per update.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, ImeEvent, KeyboardInput, MouseButton};
use regia_ui::focus::{FocusState, Focusable};
use regia_ui::localization::Localization;
use regia_ui::prelude::*;
use regia_ui::{CaretMap, ClipboardProvider, InputFilter, MemoryClipboard, TextInputPart, UiClipboard};

fn app(clipboard: &MemoryClipboard) -> App {
    let mut app = App::new();
    app.insert_resource(TextSystem::embedded_only());
    app.insert_resource(UiClipboard::new(clipboard.clone()));
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<KeyboardInput>()
        .add_message::<ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app
}

/// A root with one row; returns the row to put inputs in.
fn row(app: &mut App) -> Entity {
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(20.0),
                gap: [Dimension::Points(10.0), Dimension::Points(10.0)],
                ..Default::default()
            }),
        ))
        .id();
    let row = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                gap: [Dimension::Points(10.0), Dimension::Points(10.0)],
                ..Default::default()
            }),
        ))
        .id();
    world.entity_mut(root).add_child(row);
    row
}

/// A fixed-size input (so it scrolls) with 8 px padding.
fn input(app: &mut App, parent: Entity, width: f32, text: &str, multiline: bool) -> Entity {
    let mut field = TextInput::default();
    field.multiline = multiline;
    field.set_text(text);
    let height = if multiline { 80.0 } else { 36.0 };
    let id = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(width), Dimension::Points(height)],
                padding: Edges::px(8.0),
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            field,
        ))
        .id();
    app.world_mut().entity_mut(parent).add_child(id);
    id
}

fn settle(app: &mut App) {
    for _ in 0..4 {
        app.update();
    }
}

fn drain(app: &mut App) -> Vec<UiEvent> {
    app.world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect()
}

fn key_event(app: &mut App, logical: &str, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: logical.to_owned(),
        logical_key: logical.to_owned(),
        state,
        repeat: false,
        timestamp: Default::default(),
    });
}

/// Presses a chord like `["Control", "Shift", "ArrowLeft"]` (modifiers first) in one frame, the
/// way the platform layer delivers it, then runs one update. Returns the frame's UI events.
fn chord(app: &mut App, keys: &[&str]) -> Vec<UiEvent> {
    for k in keys {
        key_event(app, k, ButtonState::Pressed);
    }
    for k in keys.iter().rev() {
        key_event(app, k, ButtonState::Released);
    }
    app.update();
    drain(app)
}

fn type_keys(app: &mut App, text: &str) {
    for c in text.chars() {
        let name = if c == ' ' {
            "Space".to_owned()
        } else {
            c.to_string()
        };
        key_event(app, &name, ButtonState::Pressed);
        key_event(app, &name, ButtonState::Released);
    }
    app.update();
}

fn pointer(app: &mut App, p: [f32; 2]) {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (f64::from(p[0]), f64::from(p[1])),
        timestamp: Default::default(),
    });
}

fn button(app: &mut App, state: ButtonState) -> Vec<UiEvent> {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: MouseButton::Left,
        state,
        timestamp: Default::default(),
    });
    app.update();
    drain(app)
}

fn click(app: &mut App, p: [f32; 2]) -> Vec<UiEvent> {
    pointer(app, p);
    app.update();
    let mut all = button(app, ButtonState::Pressed);
    all.extend(button(app, ButtonState::Released));
    all
}

fn field(app: &App, e: Entity) -> TextInput {
    app.world().get::<TextInput>(e).unwrap().clone()
}

fn rect(app: &App, e: Entity) -> [f32; 4] {
    app.world().get::<ComputedNode>(e).unwrap().rect
}

fn label(app: &App, e: Entity) -> Entity {
    app.world()
        .get::<Children>(e)
        .unwrap()
        .iter()
        .find(|c| app.world().get::<Text>(*c).is_some())
        .unwrap()
}

/// The caret map of the input's label, built the way the widget builds it.
fn map_of(app: &mut App, e: Entity) -> CaretMap {
    let text = app.world().get::<Text>(label(app, e)).unwrap().clone();
    let world = app.world_mut();
    world.resource_scope(|world, mut fonts: Mut<TextSystem>| {
        let loc = world.resource::<Localization>();
        fonts.caret_map(&text, 1.0, None, loc)
    })
}

/// Screen point of byte `b` of a single-line input (text block at the content box, vertically
/// centered, no scroll).
fn point_of(app: &mut App, e: Entity, b: usize) -> [f32; 2] {
    let r = rect(app, e);
    let map = map_of(app, e);
    let [x, top, h] = map.caret(b);
    let y0 = (r[1] + 8.0 + ((r[3] - 16.0 - map.height()) * 0.5).max(0.0)).round();
    [(r[0] + 8.0).round() + x, y0 + top + h * 0.5]
}

fn of_kind<'a>(events: &'a [UiEvent], e: Entity) -> impl Iterator<Item = &'a UiEventKind> + 'a {
    events.iter().filter(move |ev| ev.target == e).map(|ev| &ev.kind)
}

#[test]
fn typing_selection_clipboard_and_submit_through_platform_messages() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 300.0, "", false);
    settle(&mut app);
    // A press on the field focuses it.
    let r = rect(&app, e);
    click(&mut app, [r[0] + 20.0, r[1] + 18.0]);
    assert_eq!(app.world().resource::<FocusState>().focused, Some(e));

    type_keys(&mut app, "hello world");
    let events = drain(&mut app);
    assert_eq!(field(&app, e).text, "hello world");
    assert!(
        of_kind(&events, e).any(|k| *k == UiEventKind::ValueChanged(UiValue::Text("hello world".into()))),
        "typing sends ValueChanged with the text"
    );
    let shown = app.world().get::<Text>(label(&app, e)).unwrap().spans[0]
        .content
        .clone();
    assert_eq!(shown, regia_ui::text::TextContent::Lit("hello world".into()));

    chord(&mut app, &["Control", "Shift", "ArrowLeft"]);
    assert_eq!(field(&app, e).selected_text(), "world");
    chord(&mut app, &["Control", "c"]);
    assert_eq!(clip.contents().as_deref(), Some("world"));
    chord(&mut app, &["Home"]);
    chord(&mut app, &["Control", "v"]);
    assert_eq!(field(&app, e).text, "worldhello world");
    chord(&mut app, &["Control", "z"]);
    assert_eq!(field(&app, e).text, "hello world");

    let events = chord(&mut app, &["Enter"]);
    assert!(of_kind(&events, e).any(|k| *k == UiEventKind::Submitted("hello world".into())));
}

#[test]
fn arrow_keys_edit_the_focused_input_instead_of_moving_focus() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let a = input(&mut app, parent, 200.0, "abc", false);
    let _b = input(&mut app, parent, 200.0, "xyz", false);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(a);
    chord(&mut app, &["ArrowRight"]);
    chord(&mut app, &["ArrowLeft"]);
    assert_eq!(
        app.world().resource::<FocusState>().focused,
        Some(a),
        "focus stays"
    );
    assert_eq!(field(&app, a).caret, 2);
    // Tab still moves focus.
    chord(&mut app, &["Tab"]);
    assert_ne!(app.world().resource::<FocusState>().focused, Some(a));
}

#[test]
fn caret_map_agrees_with_the_drawn_glyphs() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 300.0, "Wave hold", false);
    settle(&mut app);
    let map = map_of(&mut app, e);
    let shaped = app.world().get::<ShapedText>(label(&app, e)).unwrap().clone();
    let clusters: Vec<_> = map.lines.iter().flat_map(|l| l.clusters.iter()).collect();
    assert_eq!(clusters.len(), shaped.glyphs.len(), "one cluster per Latin glyph");
    for (c, g) in clusters.iter().zip(&shaped.glyphs) {
        assert!(
            (c.x0 - g.pen[0] as f32).abs() <= 1.0,
            "cluster at {} vs glyph pen {}",
            c.x0,
            g.pen[0]
        );
    }
}

#[test]
fn pointer_places_the_caret_drags_and_multi_clicks_select() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 300.0, "alpha beta gamma", false);
    settle(&mut app);
    // Click right on the caret boundary before "beta" (byte 6).
    let p = point_of(&mut app, e, 6);
    click(&mut app, p);
    let f = field(&app, e);
    assert_eq!((f.caret, f.selection_anchor), (6, 6));

    // Drag from byte 1 to byte 4.
    let from = point_of(&mut app, e, 1);
    let to = point_of(&mut app, e, 4);
    pointer(&mut app, from);
    app.update();
    // One more step so the press is well outside the double-click window of the last click.
    app.update();
    button(&mut app, ButtonState::Pressed);
    pointer(&mut app, [to[0], to[1]]);
    app.update();
    button(&mut app, ButtonState::Released);
    let f = field(&app, e);
    assert_eq!(f.selection(), (1, 4), "drag selects from press to pointer");

    // Double click inside "gamma" selects the word; a third click selects the line.
    let g = point_of(&mut app, e, 13);
    click(&mut app, g);
    let events = click(&mut app, g);
    assert!(of_kind(&events, e).any(|k| *k == UiEventKind::DoubleClick));
    assert_eq!(field(&app, e).selected_text(), "gamma");
    pointer(&mut app, g);
    button(&mut app, ButtonState::Pressed);
    button(&mut app, ButtonState::Released);
    assert_eq!(field(&app, e).selected_text(), "alpha beta gamma");

    // Shift+click extends from the anchor.
    chord(&mut app, &["Home"]);
    key_event(&mut app, "Shift", ButtonState::Pressed);
    let end = point_of(&mut app, e, 5);
    click(&mut app, end);
    key_event(&mut app, "Shift", ButtonState::Released);
    app.update();
    assert_eq!(field(&app, e).selection(), (0, 5));
}

/// Roadmap lane 6 acceptance ("mouse selection test on shaped Latin and Arabic text"): the same
/// pointer path as the Latin test, on right-to-left Arabic in a live input. Pixel points come from
/// the widget's own caret map, so the test checks that press, drag and double-click resolve to the
/// shaped clusters' byte offsets, not that the glyphs sit at any fixed pixel.
#[test]
fn pointer_drags_and_double_clicks_select_on_shaped_arabic_text() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    // No lam-alef: every letter is its own caret stop.
    let text = "كتاب جميل";
    let e = input(&mut app, parent, 300.0, text, false);
    settle(&mut app);
    let map = map_of(&mut app, e);
    assert!(
        map.lines[0].clusters.iter().any(|c| c.rtl),
        "the label is shaped right to left"
    );
    let at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(b, _)| b);
    // In RTL the text's start is to the right of its end.
    assert!(point_of(&mut app, e, 0)[0] > point_of(&mut app, e, at(4))[0]);

    // A click on a caret boundary inside the first word places the caret there.
    let p = point_of(&mut app, e, at(2));
    click(&mut app, p);
    let f = field(&app, e);
    assert_eq!((f.caret, f.selection_anchor), (at(2), at(2)));

    // Drag from char 1 to char 3: the pointer moves right to left, the selection is bytes 1..3.
    let from = point_of(&mut app, e, at(1));
    let to = point_of(&mut app, e, at(3));
    pointer(&mut app, from);
    app.update();
    // Out of the double-click window of the last click.
    app.update();
    button(&mut app, ButtonState::Pressed);
    pointer(&mut app, to);
    app.update();
    button(&mut app, ButtonState::Released);
    let f = field(&app, e);
    assert_eq!(
        f.selection(),
        (at(1), at(3)),
        "drag selects from press to pointer"
    );
    assert_eq!(f.selected_text(), "تا");

    // A double click inside the second word selects that word.
    let g = point_of(&mut app, e, at(7));
    click(&mut app, g);
    let events = click(&mut app, g);
    assert!(of_kind(&events, e).any(|k| *k == UiEventKind::DoubleClick));
    assert_eq!(field(&app, e).selected_text(), "جميل");
}

#[test]
fn hit_tests_on_shaped_latin_arabic_and_combining_text() {
    let mut fonts = TextSystem::embedded_only();
    let loc = Localization::default();

    let latin = fonts.caret_map(&Text::new("abc"), 1.0, None, &loc);
    let xs: Vec<f32> = (0..=3).map(|b| latin.caret(b)[0]).collect();
    assert!(
        xs.windows(2).all(|w| w[0] < w[1]),
        "LTR carets advance left to right: {xs:?}"
    );
    for b in 0..=3 {
        assert_eq!(
            latin.hit([latin.caret(b)[0], 5.0]),
            b,
            "hit at caret {b} returns it"
        );
    }
    assert_eq!(latin.hit([-50.0, 5.0]), 0);
    assert_eq!(latin.hit([500.0, 5.0]), 3);

    // Arabic is laid out right to left: the text's start is the rightmost caret.
    let arabic_text = "سلام";
    let arabic = fonts.caret_map(&Text::new(arabic_text), 1.0, None, &loc);
    assert!(arabic.lines[0].clusters.iter().all(|c| c.rtl));
    let start = arabic.caret(0)[0];
    let end = arabic.caret(arabic_text.len())[0];
    assert!(start > end, "RTL: start {start} is right of end {end}");
    assert_eq!(
        arabic.hit([start + 50.0, 5.0]),
        0,
        "right of the text is its start"
    );
    assert_eq!(
        arabic.hit([end - 50.0, 5.0]),
        arabic_text.len(),
        "left of the text is its end"
    );
    let second = arabic_text.char_indices().nth(1).unwrap().0;
    assert_eq!(arabic.hit([arabic.caret(second)[0], 5.0]), second);

    // A base with a combining mark is one caret stop: no hit lands inside it.
    let marked = "e\u{301}x";
    let map = fonts.caret_map(&Text::new(marked), 1.0, None, &loc);
    let w = map.width();
    for i in 0..=40 {
        let b = map.hit([w * i as f32 / 40.0, 5.0]);
        assert!([0, 3, 4].contains(&b), "hit {b} splits the accented e");
    }

    // Two lines: Up/Down targets and line lookup.
    let two = fonts.caret_map(&Text::new("ab\ncd"), 1.0, None, &loc);
    assert_eq!(two.lines.len(), 2);
    assert_eq!(two.line_of(2), 0, "end of line one");
    assert_eq!(two.line_of(3), 1);
    let below = two.hit([two.caret(1)[0], two.lines[1].top + 2.0]);
    assert_eq!(below, 4, "same x one line down");
    // A trailing break gets an empty last line for the caret.
    let trailing = fonts.caret_map(&Text::new("ab\n"), 1.0, None, &loc);
    assert_eq!(trailing.line_of(3), trailing.lines.len() - 1);
    assert!(trailing.caret(3)[1] > trailing.caret(0)[1]);
}

#[test]
fn long_text_scrolls_to_keep_the_caret_visible() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 80.0, "", false);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    type_keys(&mut app, "the quick brown fox jumps");
    settle(&mut app);
    let scrolled = field(&app, e).scroll[0];
    assert!(
        scrolled > 0.0,
        "caret at the end scrolled the text, got {scrolled}"
    );
    assert!(
        app.world().get::<ClipChildren>(e).is_some(),
        "a scrolling field clips"
    );
    chord(&mut app, &["Home"]);
    settle(&mut app);
    assert_eq!(field(&app, e).scroll[0], 0.0, "Home scrolls back to the start");
    // The caret node sits inside the field.
    let caret = app
        .world()
        .get::<Children>(e)
        .unwrap()
        .iter()
        .find(|c| app.world().get::<TextInputPart>(*c) == Some(&TextInputPart::Caret))
        .unwrap();
    let (cr, fr) = (rect(&app, caret), rect(&app, e));
    assert!(
        cr[0] >= fr[0] && cr[0] + cr[2] <= fr[0] + fr[2],
        "caret {cr:?} inside {fr:?}"
    );
}

#[test]
fn ime_commits_insert_and_preedit_is_kept_apart() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 200.0, "a", false);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    app.world_mut()
        .write_message(ImeEvent::Preedit(Entity::PLACEHOLDER, "ni".into()));
    app.update();
    let f = field(&app, e);
    assert_eq!((f.text.as_str(), f.preedit.as_str()), ("a", "ni"));
    app.world_mut()
        .write_message(ImeEvent::Commit(Entity::PLACEHOLDER, "你".into()));
    app.update();
    let f = field(&app, e);
    assert_eq!((f.text.as_str(), f.preedit.as_str()), ("a你", ""));
}

#[test]
fn filters_and_outside_edits_apply_to_the_live_widget() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 200.0, "", false);
    app.world_mut().get_mut::<TextInput>(e).unwrap().filter = InputFilter::Numeric;
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    type_keys(&mut app, "1a.5.");
    assert_eq!(field(&app, e).text, "1.5");
    assert!(field(&app, e).can_undo());
    // Text replaced from code drops the undo history (it no longer matches the field).
    app.world_mut().get_mut::<TextInput>(e).unwrap().text = "42".into();
    app.update();
    assert!(!field(&app, e).can_undo());
    chord(&mut app, &["Control", "z"]);
    assert_eq!(field(&app, e).text, "42");
}

/// Review rev-kimi-20 finding 2: a multi-line fixed-size field that already holds text when it is
/// spawned must word-wrap without an edit. Its first label sync runs before layout gave it a
/// `ComputedNode` (so it cannot know it scrolls yet); the idle skip must not keep that first
/// unwrapped label forever.
#[test]
fn a_multiline_field_spawned_with_text_wraps_without_an_edit() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let text = "lorem ipsum dolor sit amet ".repeat(12);
    let e = input(&mut app, parent, 200.0, &text, true);
    settle(&mut app);
    let wrap = app.world().get::<Text>(label(&app, e)).unwrap().wrap;
    assert_eq!(
        wrap,
        regia_ui::text::WrapMode::Word,
        "a 200x80 multi-line field with {} chars kept an unwrapped label",
        text.len()
    );
}

/// The same skip must notice a runtime size change: a multi-line field whose height becomes auto
/// stops scrolling, so it stops wrapping (and wraps again once the size is definite).
#[test]
fn a_multiline_field_rewraps_when_its_size_changes() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    // Text set after the first layout, so the wrap starts right whatever the spawn-time sync did.
    let e = input(&mut app, parent, 200.0, "", true);
    settle(&mut app);
    app.world_mut()
        .get_mut::<TextInput>(e)
        .unwrap()
        .set_text(&"word ".repeat(60));
    settle(&mut app);
    let wrap = |app: &App| app.world().get::<Text>(label(app, e)).unwrap().wrap;
    assert_eq!(wrap(&app), regia_ui::text::WrapMode::Word);
    app.world_mut().get_mut::<NodeStyle>(e).unwrap().0.size[1] = Dimension::Auto;
    settle(&mut app);
    assert_eq!(
        wrap(&app),
        regia_ui::text::WrapMode::None,
        "an auto-height field does not scroll, so it breaks only at line breaks"
    );
    app.world_mut().get_mut::<NodeStyle>(e).unwrap().0.size[1] = Dimension::Points(80.0);
    settle(&mut app);
    assert_eq!(wrap(&app), regia_ui::text::WrapMode::Word);
}

// ---- ADR-273 W01 part 2 (ADR-7208): IME preedit drawing, IME rect, visual (bidi) caret ----

/// The caret node's laid-out rect (window physical px).
fn caret_rect(app: &App, e: Entity) -> [f32; 4] {
    let caret = app
        .world()
        .get::<Children>(e)
        .unwrap()
        .iter()
        .find(|c| app.world().get::<TextInputPart>(*c) == Some(&TextInputPart::Caret))
        .unwrap();
    rect(app, caret)
}

/// The label's spans joined, and the spans themselves.
fn label_spans(app: &App, e: Entity) -> Vec<(String, regia_ui::text::TextStyle)> {
    let loc = Localization::default();
    app.world()
        .get::<Text>(label(app, e))
        .unwrap()
        .spans
        .iter()
        .map(|s| (s.content.resolve(&loc), s.style.clone()))
        .collect()
}

/// Roadmap lane 6 part 2: the composition in progress is drawn inline in the label at the caret,
/// underlined, and the caret sits inside it (at its end when the IME names no cursor).
#[test]
fn preedit_is_drawn_inline_underlined_with_the_caret_inside() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 300.0, "ab", false);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    chord(&mut app, &["ArrowLeft"]);
    assert_eq!(field(&app, e).caret, 1);
    app.world_mut()
        .write_message(ImeEvent::Preedit(Entity::PLACEHOLDER, "ni".into()));
    settle(&mut app);
    assert_eq!(field(&app, e).text, "ab", "the preedit is not part of the text");
    let spans = label_spans(&app, e);
    let joined: String = spans.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(
        joined, "anib",
        "the label shows the preedit at the caret: {spans:?}"
    );
    let pre = spans
        .iter()
        .position(|(s, _)| s == "ni")
        .expect("the preedit is its own span");
    assert!(spans[pre].1.underline, "the preedit span is underlined");
    assert!(
        spans
            .iter()
            .filter(|(s, _)| s != "ni")
            .all(|(_, st)| !st.underline),
        "only the preedit is underlined"
    );
    // The shaped (drawn) label has the preedit glyphs and an underline over exactly them.
    let shaped = app.world().get::<ShapedText>(label(&app, e)).unwrap().clone();
    assert_eq!(shaped.glyphs.len(), 4, "a, n, i, b are shaped");
    let map = map_of(&mut app, e);
    let (x0, x1) = (map.caret(1)[0], map.caret(3)[0]);
    let underline = shaped
        .decorations
        .iter()
        .find(|d| !d.strike && usize::from(d.span) == pre)
        .expect("an underline rect for the preedit span");
    assert!(
        (underline.rect[0] - x0).abs() <= 1.0 && (underline.rect[0] + underline.rect[2] - x1).abs() <= 1.0,
        "underline {:?} spans the preedit {x0}..{x1}",
        underline.rect
    );
    // The caret is drawn at the preedit's end (display byte 3), inside the label.
    let label_x = rect(&app, label(&app, e))[0];
    let cx = caret_rect(&app, e)[0];
    assert!(
        (cx - (label_x + x1)).abs() <= 1.5,
        "caret at {cx}, preedit end at {}",
        label_x + x1
    );
    // Committing replaces the preedit with the text; the label is one plain span again.
    app.world_mut()
        .write_message(ImeEvent::Commit(Entity::PLACEHOLDER, "你".into()));
    settle(&mut app);
    assert_eq!(field(&app, e).text, "a你b");
    let spans = label_spans(&app, e);
    assert_eq!(spans.len(), 1);
    assert!(!spans[0].1.underline);
}

/// Roadmap lane 6 part 2: Right moves the caret one visual step right across a left-to-right /
/// right-to-left boundary (the logical offset jumps), Left walks back the same stops.
#[test]
fn right_arrow_moves_visually_across_an_ltr_rtl_boundary() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    // "ab " then three Arabic letters then " cd": an LTR paragraph with one RTL run.
    let text = "ab كتب cd";
    let e = input(&mut app, parent, 400.0, text, false);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    chord(&mut app, &["Home"]);
    settle(&mut app);
    let at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(b, _)| b);
    let mut xs = vec![caret_rect(&app, e)[0]];
    let mut bytes = vec![field(&app, e).caret];
    for _ in 0..text.chars().count() {
        chord(&mut app, &["ArrowRight"]);
        settle(&mut app);
        xs.push(caret_rect(&app, e)[0]);
        bytes.push(field(&app, e).caret);
    }
    assert!(
        xs.windows(2).all(|w| w[1] > w[0]),
        "every Right press moves the caret right on screen: {xs:?} (bytes {bytes:?})"
    );
    // Visual order: a b space | ب ت ك | space c d. Entering the RTL run from the left lands on
    // its logical end side, so the offsets inside it go backwards.
    assert_eq!(
        bytes,
        vec![0, 1, 2, at(3), at(5), at(4), at(6), at(7), at(8), at(9)],
        "visual stops in logical bytes"
    );
    // Left walks the same stops back.
    let mut back = vec![field(&app, e).caret];
    for _ in 0..text.chars().count() {
        chord(&mut app, &["ArrowLeft"]);
        settle(&mut app);
        back.push(field(&app, e).caret);
    }
    back.reverse();
    assert_eq!(back, bytes, "Left retraces Right");
}

/// Roadmap lane 6: a pointer drag over the Arabic run of a mixed-direction field selects the
/// logical range between the pressed and the released caret boundaries.
#[test]
fn a_drag_over_arabic_in_mixed_text_selects_the_logical_range_under_the_pointer() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let text = "ab كتاب جميل cd";
    let e = input(&mut app, parent, 400.0, text, false);
    settle(&mut app);
    let at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(b, _)| b);
    // From inside the first Arabic word to inside the second one.
    let from = point_of(&mut app, e, at(4));
    let to = point_of(&mut app, e, at(10));
    assert!(from[0] > to[0], "the RTL run reads right to left on screen");
    pointer(&mut app, from);
    app.update();
    button(&mut app, ButtonState::Pressed);
    pointer(&mut app, to);
    app.update();
    button(&mut app, ButtonState::Released);
    let f = field(&app, e);
    assert_eq!(
        f.selection(),
        (at(4), at(10)),
        "the logical range under the pointer"
    );
    assert_eq!(f.selected_text(), "تاب جم");
}

/// Roadmap lane 6 part 2: the IME's cursor inside the composition places the caret there.
#[test]
fn preedit_cursor_from_the_ime_places_the_caret_inside_the_composition() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 300.0, "ab", false);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    app.world_mut()
        .write_message(ImeEvent::Preedit(Entity::PLACEHOLDER, "xyz".into()));
    app.world_mut()
        .write_message(ImeEvent::PreeditCursor(Entity::PLACEHOLDER, Some((1, 1))));
    settle(&mut app);
    assert_eq!(field(&app, e).preedit_cursor, Some((1, 1)));
    let map = map_of(&mut app, e);
    let label_x = rect(&app, label(&app, e))[0];
    // "ab" + "xyz": the IME cursor after "x" is label byte 3.
    let want = label_x + map.caret(3)[0];
    let cx = caret_rect(&app, e)[0];
    assert!((cx - want).abs() <= 1.5, "caret at {cx}, IME cursor at {want}");
    // While composing, the IME rect spans the composition (label bytes 2..5).
    let ime = field(&app, e).ime_rect().expect("an IME rect while focused");
    let (x0, x1) = (label_x + map.caret(2)[0], label_x + map.caret(5)[0]);
    assert!(
        (ime[0] - x0).abs() <= 1.5 && (ime[0] + ime[2] - x1).abs() <= 1.5,
        "IME rect {ime:?} over the composition {x0}..{x1}"
    );
}

/// review fixrev7 finding 1: blurring a field mid-composition clears the composition (and its
/// ghost underlined label); the unfocused field must not keep drawing the preedit.
#[test]
fn blurring_mid_composition_clears_the_preedit_and_its_underline() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let a = input(&mut app, parent, 200.0, "ab", false);
    let b = input(&mut app, parent, 200.0, "", false);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(a);
    app.world_mut()
        .write_message(ImeEvent::Preedit(Entity::PLACEHOLDER, "ni".into()));
    app.update();
    assert_eq!(field(&app, a).preedit, "ni");
    let spans = label_spans(&app, a);
    assert!(
        spans.iter().any(|(_, st)| st.underline),
        "the composition shows underlined: {spans:?}"
    );
    // Focus moves to the other field (click, Tab, agent): the composition is abandoned.
    app.world_mut().resource_mut::<FocusState>().focused = Some(b);
    settle(&mut app);
    let f = field(&app, a);
    assert_eq!(f.preedit, "", "blur clears the abandoned composition");
    assert_eq!(f.preedit_cursor, None, "blur clears the IME cursor too");
    let spans = label_spans(&app, a);
    assert_eq!(spans.len(), 1, "the label is one plain span again: {spans:?}");
    assert_eq!(spans[0].0, "ab", "no ghost composition text");
    assert!(!spans[0].1.underline, "no underline left on the committed label");
}

/// review fixrev7 finding 2: moving the caret to offset 0 mid-composition must not corrupt the
/// label's base style — the underlined composition span at index 0 must never be picked as the
/// style to restore after the commit.
#[test]
fn caret_at_zero_mid_composition_does_not_underline_the_label_after_commit() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 300.0, "abcd", false);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    app.world_mut()
        .write_message(ImeEvent::Preedit(Entity::PLACEHOLDER, "xyz".into()));
    settle(&mut app);
    // The caret moves to offset 0 while the IME keeps composing (arrow keys reach the field
    // during IME sessions); the IME then updates the composition string.
    chord(&mut app, &["Home"]);
    settle(&mut app);
    app.world_mut()
        .write_message(ImeEvent::Preedit(Entity::PLACEHOLDER, "xyza".into()));
    settle(&mut app);
    app.world_mut()
        .write_message(ImeEvent::Commit(Entity::PLACEHOLDER, "你".into()));
    settle(&mut app);
    let f = field(&app, e);
    assert_eq!(f.text, "你abcd", "committed at the caret");
    assert_eq!(f.preedit, "");
    let spans = label_spans(&app, e);
    assert_eq!(spans.len(), 1, "one span after the commit: {spans:?}");
    assert_eq!(spans[0].0, "你abcd");
    assert!(
        !spans[0].1.underline,
        "the committed label is not underlined: {spans:?}"
    );
}

/// review fixrev7 finding 3: an IME composition that wraps a visual line reports an IME rect on
/// the caret's (last) line only — the width must not mix x coordinates of two lines.
#[test]
fn ime_rect_stays_on_the_carets_line_when_the_composition_wraps() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 100.0, "", true);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    // Long enough to wrap over several visual lines of a 100 px field.
    app.world_mut().write_message(ImeEvent::Preedit(
        Entity::PLACEHOLDER,
        "one two three four five six seven eight nine ten".into(),
    ));
    settle(&mut app);
    let f = field(&app, e);
    assert!(
        f.scroll[1] > 0.0,
        "the wrapped composition is taller than the field (scrolls vertically)"
    );
    let ime = f.ime_rect().expect("an IME rect while composing");
    let caret = caret_rect(&app, e);
    assert!(
        (ime[1] - caret[1]).abs() <= 1.0,
        "the rect is on the caret's line: {ime:?} vs caret {caret:?}"
    );
    // The composition ends at the caret, so the rect's right edge is the drawn caret: a width
    // mixed from the first line's x would stick out past it.
    assert!(
        (ime[0] + ime[2] - caret[0]).abs() <= 1.5,
        "the rect ends at the caret on the caret's line: ime {ime:?}, caret x {}",
        caret[0]
    );
}

/// Roadmap lane 6 part 2: the rect reported for the IME candidate window follows the caret as
/// the user types and as the field scrolls, and is handed to the platform layer.
#[test]
fn ime_rect_follows_the_caret_after_typing_and_scrolling() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 120.0, "", false);
    settle(&mut app);
    assert_eq!(field(&app, e).ime_rect(), None, "no rect while unfocused");
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    settle(&mut app);
    let area = |app: &App| app.world().resource::<regia_platform::ImeCursorArea>().rect;
    let empty = field(&app, e).ime_rect().expect("focused field has a rect");
    assert_eq!(
        area(&app),
        Some(empty),
        "the rect is handed to the platform layer"
    );
    type_keys(&mut app, "ab");
    settle(&mut app);
    let typed = field(&app, e).ime_rect().unwrap();
    assert!(
        typed[0] > empty[0],
        "typing moved the rect right: {empty:?} -> {typed:?}"
    );
    assert!(
        (typed[0] - caret_rect(&app, e)[0]).abs() <= 1.0,
        "the rect is the drawn caret"
    );
    let caret = field(&app, e).visual_caret().expect("drawn caret");
    assert!((caret[0] - typed[0]).abs() <= f32::EPSILON);
    // Long text scrolls the field; the rect stays on the caret, inside the field.
    type_keys(&mut app, " the quick brown fox jumps over");
    settle(&mut app);
    let f = field(&app, e);
    assert!(f.scroll[0] > 0.0, "the field scrolled");
    let far = f.ime_rect().unwrap();
    let fr = rect(&app, e);
    assert!(
        far[0] >= fr[0] && far[0] + far[2] <= fr[0] + fr[2],
        "rect {far:?} inside the field {fr:?}"
    );
    assert!((far[0] - caret_rect(&app, e)[0]).abs() <= 1.0);
    // Home scrolls back: the rect returns to the start.
    chord(&mut app, &["Home"]);
    settle(&mut app);
    let home = field(&app, e).ime_rect().unwrap();
    assert!(
        (home[0] - empty[0]).abs() <= 1.0,
        "back at the start: {home:?} vs {empty:?}"
    );
    assert_eq!(area(&app), Some(home));
    // Moving the field moves the rect with it.
    app.world_mut().get_mut::<NodeStyle>(parent).unwrap().0.padding = Edges::px(30.0);
    app.world_mut()
        .resource_mut::<regia_ui::layout::LayoutDirty>()
        .mark();
    settle(&mut app);
    let moved = field(&app, e).ime_rect().unwrap();
    assert!(
        (moved[0] - home[0] - 30.0).abs() <= 1.0 && (moved[1] - home[1] - 30.0).abs() <= 1.0,
        "the rect follows the field: {home:?} -> {moved:?}"
    );
    // Focus leaves: no rect.
    app.world_mut().resource_mut::<FocusState>().focused = None;
    settle(&mut app);
    assert_eq!(field(&app, e).ime_rect(), None);
    assert_eq!(area(&app), None);
}

/// ADR-7208: in a single-direction line the visual stops are the logical offsets in reading
/// order: ascending in left-to-right text, descending in right-to-left text, x always ascending.
#[test]
fn visual_stops_follow_the_reading_order_of_single_direction_lines() {
    let mut fonts = TextSystem::embedded_only();
    let loc = Localization::default();
    let latin = fonts.caret_map(&Text::new("abc"), 1.0, None, &loc);
    let stops = latin.visual_stops(0);
    assert_eq!(stops.iter().map(|s| s.1).collect::<Vec<_>>(), [0, 1, 2, 3]);
    assert!(!latin.line_rtl(0));
    let arabic_text = "سلام";
    let arabic = fonts.caret_map(&Text::new(arabic_text), 1.0, None, &loc);
    assert!(arabic.line_rtl(0));
    let stops = arabic.visual_stops(0);
    let bytes: Vec<usize> = stops.iter().map(|s| s.1).collect();
    let mut want: Vec<usize> = arabic_text.char_indices().map(|(b, _)| b).collect();
    want.push(arabic_text.len());
    want.reverse();
    assert_eq!(bytes, want, "right to left: the text's end is the leftmost stop");
    for map in [&latin, &arabic] {
        let xs: Vec<f32> = map.visual_stops(0).iter().map(|s| s.0).collect();
        assert!(xs.windows(2).all(|w| w[1] > w[0]), "x ascends: {xs:?}");
    }
    // A step from the end of the Arabic word (its left edge) to the right reaches the last
    // letter's start; from the start (right edge) there is no stop further right.
    let end_x = arabic.caret(arabic_text.len())[0];
    let last = arabic_text.char_indices().last().map(|(b, _)| b).unwrap();
    assert_eq!(
        arabic.visual_step(arabic_text.len(), end_x, true).map(|s| s.1),
        Some(last)
    );
    assert_eq!(arabic.visual_step(0, arabic.caret(0)[0], true), None);
}

// ---- post05w3 (ADR-9340): visual word moves, visual Home/End, the paragraph's base level ----

/// Presses `keys` (one chord) and returns the caret byte and the drawn caret x afterwards.
fn press_at(app: &mut App, e: Entity, keys: &[&str]) -> (usize, f32) {
    chord(app, keys);
    settle(app);
    (field(app, e).caret, caret_rect(app, e)[0])
}

/// A focused single-line field holding `text`, settled.
fn focused_field(app: &mut App, text: &str) -> Entity {
    let parent = row(app);
    let e = input(app, parent, 400.0, text, false);
    settle(app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    settle(app);
    e
}

/// Places the caret at byte `b` (no selection) without a key move.
fn caret_to(app: &mut App, e: Entity, b: usize) {
    app.world_mut().get_mut::<TextInput>(e).unwrap().select(b, b);
    settle(app);
}

/// ADR-9340 (1): Ctrl+Right in right-to-left text moves right on screen to the next word's
/// right edge, which in Arabic is the word's logical start; Ctrl+Left mirrors it.
#[test]
fn ctrl_right_in_arabic_lands_on_the_next_visual_word_boundary() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    // Three Arabic words: reading right to left, the text's end is the leftmost position.
    let text = "كلمة اخرى ثالثة";
    let e = focused_field(&mut app, text);
    let starts: Vec<usize> = text
        .split(' ')
        .scan(0, |at, w| {
            let s = *at;
            *at += w.len() + 1;
            Some(s)
        })
        .collect();
    caret_to(&mut app, e, text.len());
    let mut x = caret_rect(&app, e)[0];
    let mut bytes = Vec::new();
    for _ in 0..3 {
        let (b, nx) = press_at(&mut app, e, &["Control", "ArrowRight"]);
        assert!(
            nx > x,
            "Ctrl+Right moves right on screen: {x} -> {nx} (bytes {bytes:?}, now {b})"
        );
        bytes.push(b);
        x = nx;
    }
    assert_eq!(
        bytes,
        vec![starts[2], starts[1], starts[0]],
        "each word's right edge on screen is its logical start"
    );
    // Ctrl+Left walks back to each word's left edge on screen: the word's logical end.
    let ends = [starts[1] - 1, starts[2] - 1, text.len()];
    let mut back = Vec::new();
    for _ in 0..3 {
        let (b, nx) = press_at(&mut app, e, &["Control", "ArrowLeft"]);
        assert!(nx < x, "Ctrl+Left moves left on screen: {x} -> {nx}");
        back.push(b);
        x = nx;
    }
    assert_eq!(back, ends, "each word's left edge on screen is its logical end");
}

/// ADR-9340 (1): in a left-to-right line with a right-to-left run of two words, Ctrl+Right
/// from before the run lands on the right edge of the run's leftmost word on screen (the
/// logically *second* Arabic word's start), not on the logical end of the first word.
#[test]
fn ctrl_right_steps_through_a_mixed_line_in_screen_order() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let text = "ab كتب قلم cd";
    let e = focused_field(&mut app, text);
    let at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(b, _)| b);
    caret_to(&mut app, e, 0);
    let mut x = caret_rect(&app, e)[0];
    let mut bytes = Vec::new();
    for _ in 0..4 {
        let (b, nx) = press_at(&mut app, e, &["Control", "ArrowRight"]);
        assert!(
            nx > x,
            "Ctrl+Right moves right on screen: {x} -> {nx} (bytes {bytes:?})"
        );
        bytes.push(b);
        x = nx;
    }
    // Screen: a b _ | م ل ق _ ب ت ك | _ c d. Right edges: "ab", "قلم" (its start, at(7)), "كتب"
    // (where the run ends on screen: the left-to-right space's start, at(10)), "cd".
    assert_eq!(bytes, vec![2, at(7), at(10), text.len()]);
}

/// The drawn caret x of every logical caret position of `text` (each char boundary, placed
/// without a key move).
fn logical_caret_xs(app: &mut App, e: Entity, text: &str) -> Vec<f32> {
    let mut bounds: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    bounds.push(text.len());
    bounds
        .into_iter()
        .map(|b| {
            caret_to(app, e, b);
            caret_rect(app, e)[0]
        })
        .collect()
}

/// ADR-9340 (1): End in a right-to-left paragraph that ends with a Latin word draws the caret
/// at the line's visual end (its left edge), not inside the line where the Latin word's
/// logical end sits; Home draws it at the right edge.
#[test]
fn end_lands_on_the_visual_end_of_a_right_to_left_line() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let text = "مرحبا بالعالم abc";
    let e = focused_field(&mut app, text);
    let xs = logical_caret_xs(&mut app, e, text);
    let (lo, hi) = xs
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
    let (end_b, end_x) = press_at(&mut app, e, &["End"]);
    assert_eq!(end_b, text.len(), "End is the text's end");
    assert!(
        end_x <= lo + 0.5,
        "End is the visual end (the leftmost caret position): {end_x} vs {xs:?}"
    );
    let (home_b, home_x) = press_at(&mut app, e, &["Home"]);
    assert_eq!(home_b, 0);
    assert!(
        home_x >= hi - 0.5,
        "Home is the visual start (the rightmost caret position): {home_x} vs {xs:?}"
    );
}

/// ADR-9340 (2): "123 مرحبا" is a right-to-left paragraph (UAX #9 P2/P3: the first strong
/// character is Arabic; digits are weak), so Home goes to the right edge of the line even though
/// the line opens with a number.
///
/// A multi-line field (wrapping layout); the single-line case is the next-but-one test, after
/// the no-wrap layout fix it depends on.
#[test]
fn home_in_a_right_to_left_paragraph_that_opens_with_a_number_goes_to_the_right_edge() {
    let mut fonts = TextSystem::embedded_only();
    let loc = Localization::default();
    let map = fonts.caret_map(&Text::new("123 مرحبا"), 1.0, None, &loc);
    assert!(
        map.line_rtl(0),
        "the paragraph's base level is right to left (first strong character: Arabic)"
    );
    assert!(
        !fonts
            .caret_map(&Text::new("123 abc"), 1.0, None, &loc)
            .line_rtl(0)
    );

    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let text = "123 مرحبا";
    let parent = row(&mut app);
    let e = input(&mut app, parent, 400.0, text, true);
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    settle(&mut app);
    let xs = logical_caret_xs(&mut app, e, text);
    let (lo, hi) = xs
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
    let (home_b, home_x) = press_at(&mut app, e, &["Home"]);
    assert_eq!(home_b, 0, "Home is the paragraph's start");
    assert!(
        home_x > hi + 0.5,
        "Home is the right-to-left visual start, right of every number digit: {home_x} vs {xs:?}"
    );
    let (end_b, end_x) = press_at(&mut app, e, &["End"]);
    assert_eq!(end_b, text.len());
    assert!(
        end_x <= lo + 0.5,
        "End is the leftmost position: {end_x} vs {xs:?}"
    );
}

/// ADR-9340: cosmic-text 0.19's no-wrap layout dropped a number that opens a right-to-left
/// paragraph (`123 مرحبا` laid out as ` مرحبا`). A no-wrap text keeps every glyph and caret of
/// it, on one line, as wide as the wrapping layout measures it, right-aligned in a wider box.
#[test]
fn a_number_that_opens_a_right_to_left_paragraph_is_laid_out_without_wrapping() {
    let mut fonts = TextSystem::embedded_only();
    let loc = Localization::default();
    let wrapped = Text::new("123 مرحبا");
    let single = wrapped.clone().with_wrap(regia_ui::text::WrapMode::None);
    let want = fonts.shape(&wrapped, 1.0, None, &loc);
    for width in [None, Some(200.0f32)] {
        let shaped = fonts.shape(&single, 1.0, width, &loc);
        assert_eq!(
            shaped.glyphs.len(),
            want.glyphs.len(),
            "every glyph is laid out at {width:?}"
        );
        assert!(
            (shaped.size[0] - want.size[0]).abs() < 0.5 && shaped.lines.len() == 1,
            "one line as wide as the text at {width:?}: {:?} vs {:?}",
            shaped.size,
            want.size
        );
        let map = fonts.caret_map(&single, 1.0, width, &loc);
        assert_eq!(map.lines.len(), 1);
        let line = &map.lines[0];
        assert!(line.rtl);
        for b in 0..3 {
            assert!(
                line.clusters.iter().any(|c| c.start == b),
                "digit at byte {b} has a caret cluster at {width:?}"
            );
        }
        let right = line.clusters.iter().map(|c| c.x1).fold(0.0, f32::max);
        let edge = width.unwrap_or(want.size[0]);
        assert!(
            (right - edge).abs() < 0.5,
            "right-aligned: the line ends at {right}, the box at {edge}"
        );
    }
}

/// ADR-9340 (2) in a single-line field: Home in "123 مرحبا" goes to the right edge, right of
/// the number (the field lays the number out, see the test above).
#[test]
fn home_in_a_single_line_right_to_left_field_that_opens_with_a_number_goes_to_the_right_edge() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let text = "123 مرحبا";
    let e = focused_field(&mut app, text);
    let xs = logical_caret_xs(&mut app, e, text);
    let hi = xs.iter().fold(f32::MIN, |hi, x| hi.max(*x));
    let (home_b, home_x) = press_at(&mut app, e, &["Home"]);
    assert_eq!(home_b, 0);
    assert!(
        home_x > hi + 0.5,
        "Home is right of every digit: {home_x} vs {xs:?}"
    );
}

/// Spawns the first child of a `.ui.ron` document's root under `parent` (the loader's own path).
fn spawn_from_ui_ron(app: &mut App, parent: Entity, source: &str) -> Entity {
    let file = regia_ui::parse_ui_ron(source, "test.ui.ron", &regia_ui::UiMigrators::default())
        .unwrap_or_else(|e| panic!("parse: {e}"));
    let def = file.root.children.first().expect("one child").clone();
    regia_ui::authoring::spawn_node(app.world_mut(), parent, &def)
}

/// `.ui.ron` v11 (ADR-9980): a `text_input` with `max_chars: 4` refuses a fifth character, typed
/// or pasted, and its integer filter and read-only flag reach the live widget.
#[test]
fn ui_ron_text_input_max_chars_refuses_a_fifth_character() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = spawn_from_ui_ron(
        &mut app,
        parent,
        r#"(version: 11, root: (children: [
            (style: (size: (200, 36), padding: 8), widget: text_input("PIN", false, (max_chars: 4, filter: integer))),
        ]))"#,
    );
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    type_keys(&mut app, "12345");
    assert_eq!(field(&app, e).text, "1234", "the fifth character is refused");
    chord(&mut app, &["Backspace"]);
    type_keys(&mut app, "a9");
    assert_eq!(field(&app, e).text, "1239", "the integer filter refuses a letter");
    regia_ui::ClipboardProvider::set_text(&mut clip.clone(), "77");
    chord(&mut app, &["Control", "v"]);
    assert_eq!(
        field(&app, e).text,
        "1239",
        "a full field takes nothing from a paste"
    );
    assert_eq!(field(&app, e).placeholder, "PIN");

    let r = spawn_from_ui_ron(
        &mut app,
        parent,
        r#"(version: 11, root: (children: [
            (style: (size: (200, 36), padding: 8), widget: text_input("", false, (read_only: true))),
        ]))"#,
    );
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(r);
    type_keys(&mut app, "x");
    assert_eq!(field(&app, r).text, "", "a read-only field takes no typing");
    assert!(field(&app, r).read_only);
}

/// `.ui.ron` v11 (ADR-9980): a `secret` field draws its mask character and refuses copy and cut.
#[test]
fn ui_ron_text_input_secret_draws_the_mask_and_refuses_copy() {
    let clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = spawn_from_ui_ron(
        &mut app,
        parent,
        r#"(version: 11, root: (children: [
            (style: (size: (200, 36), padding: 8), widget: text_input("Password", false, (secret: '*'))),
        ]))"#,
    );
    settle(&mut app);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
    type_keys(&mut app, "hunter2");
    assert_eq!(field(&app, e).text, "hunter2");
    let shown = app.world().get::<Text>(label(&app, e)).unwrap().spans[0]
        .content
        .clone();
    assert_eq!(
        shown,
        regia_ui::text::TextContent::Lit("*******".into()),
        "the label draws the mask"
    );
    regia_ui::ClipboardProvider::set_text(&mut clip.clone(), "before");
    chord(&mut app, &["Control", "a"]);
    chord(&mut app, &["Control", "c"]);
    assert_eq!(clip.contents().as_deref(), Some("before"), "copy is refused");
    chord(&mut app, &["Control", "x"]);
    assert_eq!(clip.contents().as_deref(), Some("before"), "cut is refused");
    assert_eq!(field(&app, e).text, "hunter2", "a refused cut keeps the text");
}

/// rc1 UI-text finding 1: U+2028/U+2029 are not control characters, so they used to survive the
/// field's sanitize — and the label pipeline normalizes them to `\n`, shaping a *single-line*
/// field's label as two paragraphs, while the caret map's raw-`\n` line starts reported offsets
/// past the end of the text. Pasting web/JSON clipboard content must behave like pasting `\n`.
#[test]
fn unicode_separators_paste_as_one_line_in_single_line_fields() {
    let mut clip = MemoryClipboard::default();
    let mut app = app(&clip);
    let parent = row(&mut app);
    let e = input(&mut app, parent, 300.0, "", false);
    settle(&mut app);
    let r = rect(&app, e);
    click(&mut app, [r[0] + 20.0, r[1] + 18.0]);
    assert_eq!(app.world().resource::<FocusState>().focused, Some(e));

    clip.set_text("a\u{2028}b");
    chord(&mut app, &["Control", "v"]);
    assert_eq!(field(&app, e).text, "a b");
    let shown = app.world().get::<Text>(label(&app, e)).unwrap().spans[0]
        .content
        .clone();
    assert_eq!(shown, regia_ui::text::TextContent::Lit("a b".into()));

    // The label of a single-line field must shape as exactly one visual line.
    let text = app.world().get::<Text>(label(&app, e)).unwrap().clone();
    let lines = app
        .world_mut()
        .resource_scope(|world, mut fonts: Mut<TextSystem>| {
            let loc = world.resource::<Localization>();
            fonts.shape(&text, 1.0, None, loc).lines.len()
        });
    assert_eq!(lines, 1, "a single-line field's label must stay one paragraph");

    // Multi-line fields keep a real break, and the caret map covers 'b' at its raw offset.
    let e2 = input(&mut app, parent, 300.0, "", true);
    settle(&mut app);
    app.world_mut()
        .get_mut::<TextInput>(e2)
        .unwrap()
        .set_text("a\u{2028}b");
    settle(&mut app);
    assert_eq!(field(&app, e2).text, "a\nb");
    let map = map_of(&mut app, e2);
    assert_eq!(map.text, "a\nb");
    let b_at = map.text.rfind('b').unwrap_or(0);
    let Some(line_two) = map.lines.get(1) else {
        panic!("expected a second line");
    };
    assert_eq!(
        (line_two.start, line_two.end),
        (b_at, map.text.len()),
        "the second line must cover 'b' at its raw offset"
    );
    for line in &map.lines {
        for c in &line.clusters {
            assert!(
                c.start <= c.end && c.end <= map.text.len(),
                "cluster {c:?} indexes past the end of the text"
            );
        }
    }
}
