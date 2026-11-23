// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): the `TextInput` component's code-facing API on hostile text and
//! positions: sanitizing, the length limit on whole grapheme clusters, selection snapping and
//! public fields set out of range. No ECS app, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_ui::TextInput;

fn input(multiline: bool, max_chars: usize) -> TextInput {
    let mut input = TextInput::default();
    input.multiline = multiline;
    input.max_chars = max_chars;
    input
}

#[test]
fn set_text_replaces_line_breaks_and_drops_control_characters() {
    let mut single = input(false, 0);
    single.set_text("a\r\nb\nc\rd\te");
    assert_eq!(
        single.text, "a b c d e",
        "every break is one space in a single-line field"
    );
    single.set_text("a\u{0}\u{7}\u{7f}\u{1b}b\u{85}c");
    assert_eq!(single.text, "abc", "control characters are dropped");
    let mut multi = input(true, 0);
    multi.set_text("a\r\nb\rc\n\nd");
    assert_eq!(multi.text, "a\nb\nc\n\nd", "CRLF and CR become one newline each");
    // Joiners and variation selectors are not controls: emoji sequences survive.
    let family = "👨\u{200d}👩\u{200d}👧";
    multi.set_text(&format!("x{family}y\u{fe0f}"));
    assert_eq!(multi.text, format!("x{family}y\u{fe0f}"));
}

#[test]
fn set_text_puts_the_caret_at_the_end_and_clears_the_selection() {
    let mut t = input(false, 0);
    t.set_text("héllo");
    assert_eq!((t.caret, t.selection_anchor), (6, 6));
    assert_eq!(t.selected_text(), "");
    t.select_all();
    assert_eq!(t.selected_text(), "héllo");
    t.set_text("");
    assert_eq!((t.caret, t.selection_anchor, t.text.as_str()), (0, 0, ""));
    assert!(!t.can_undo() && !t.can_redo());
    assert!(!t.undo() && !t.redo(), "nothing to undo or redo");
}

#[test]
fn the_length_limit_counts_scalars_and_never_splits_a_cluster() {
    let mut t = input(false, 3);
    t.set_text("ab😀cd");
    assert_eq!(t.text, "ab😀");
    t.set_text("abc");
    assert_eq!(t.text, "abc", "exactly at the limit");
    t.set_text("abcd");
    assert_eq!(t.text, "abc");
    // "e" + COMBINING ACUTE is one cluster of two scalars.
    t.set_text("e\u{301}e\u{301}");
    assert_eq!(t.text, "e\u{301}", "the second cluster would reach four scalars");
    // A single cluster longer than the limit leaves nothing rather than half a cluster.
    t.set_text("👨\u{200d}👩\u{200d}👧");
    assert_eq!(t.text, "");
    assert_eq!(t.caret, 0);
    // Zero means unlimited.
    let mut free = input(false, 0);
    free.set_text(&"x".repeat(10_000));
    assert_eq!(free.text.len(), 10_000);
    // A limit of one keeps the first cluster only.
    let mut one = input(false, 1);
    one.set_text("日本語");
    assert_eq!(one.text, "日");
}

#[test]
fn selections_snap_to_cluster_boundaries_and_clamp() {
    let mut t = input(false, 0);
    // "é" is two bytes, the face emoji four.
    t.set_text("é😀x");
    t.select(1, 3);
    assert_eq!(
        (t.selection_anchor, t.caret),
        (0, 2),
        "inside a character snaps back to its start"
    );
    assert_eq!(t.selected_text(), "é");
    t.select(99, 99);
    assert_eq!((t.selection_anchor, t.caret), (t.text.len(), t.text.len()));
    t.select(t.text.len(), 0);
    assert_eq!(
        t.selection(),
        (0, t.text.len()),
        "selection() is ordered whichever way it was made"
    );
    assert_eq!(t.selected_text(), "é😀x");
    // "e" + combining accent is one cluster: a boundary inside it snaps to its start.
    t.set_text("e\u{301}z");
    t.select(1, 3);
    assert_eq!((t.selection_anchor, t.caret), (0, 3));
}

#[test]
fn public_positions_set_out_of_range_never_panic() {
    let mut t = input(false, 0);
    t.set_text("é😀");
    for (anchor, caret) in [
        (0, 99),
        (99, 0),
        (1, 2),
        (3, 5),
        (usize::MAX, usize::MAX),
        (usize::MAX, 0),
    ] {
        t.selection_anchor = anchor;
        t.caret = caret;
        let (a, b) = t.selection();
        assert!(a <= b);
        // Out-of-range or mid-character offsets read as no selection, not as a panic.
        let _ = t.selected_text();
        let _ = t.display_text();
        t.select_all();
        assert_eq!(t.selected_text(), "é😀");
    }
    t.selection_anchor = 1;
    t.caret = 3;
    assert_eq!(t.selected_text(), "", "offsets inside characters select nothing");
}

#[test]
fn secret_fields_mask_every_character_and_leave_the_text_alone() {
    let mut t = input(false, 0);
    t.secret = Some('*');
    t.set_text("pass😀");
    assert_eq!(t.display_text(), "*****");
    assert_eq!(t.text, "pass😀");
    t.secret = None;
    assert_eq!(t.display_text(), "pass😀");
    t.set_text("");
    t.secret = Some('•');
    assert_eq!(t.display_text(), "");
}

#[test]
fn read_only_fields_do_not_undo_or_redo() {
    let mut t = input(false, 0);
    t.read_only = true;
    t.set_text("fixed");
    assert!(!t.undo() && !t.redo());
    assert_eq!(t.text, "fixed");
}

/// Idle fields must not rebuild their label string, and a focused field must not clone its caret
/// map, on frames with no key events.
#[test]
fn idle_text_fields_do_not_rebuild_labels_or_clone_the_caret_map() {
    use std::time::Duration;

    use regia_app::TimeUpdateStrategy;
    use regia_app::prelude::*;
    use regia_ui::focus::Focusable;
    use regia_ui::prelude::*;
    use regia_ui::style::FlexDirection;
    use regia_ui::{UiHotPathCounts, UiScales};

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
    app.world_mut().resource_mut::<UiScales>().window_size = [800.0, 600.0];

    let root = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Points(400.0), Dimension::Points(600.0)],
                ..Default::default()
            }),
        ))
        .id();
    let mut secret = TextInput::default();
    secret.secret = Some('*');
    secret.set_text(&"p".repeat(4_000));
    let secret_id = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(200.0), Dimension::Points(32.0)],
                ..Default::default()
            }),
            secret,
            ChildOf(root),
        ))
        .id();
    let mut focused_field = TextInput::default();
    focused_field.set_text(&"a".repeat(2_000));
    let focused = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(200.0), Dimension::Points(32.0)],
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            focused_field,
            ChildOf(root),
        ))
        .id();
    for _ in 0..6 {
        let mut field = TextInput::default();
        field.set_text("idle");
        app.world_mut().spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(200.0), Dimension::Points(32.0)],
                ..Default::default()
            }),
            field,
            ChildOf(root),
        ));
    }
    for _ in 0..4 {
        app.update();
    }
    app.world_mut()
        .resource_mut::<regia_ui::focus::FocusState>()
        .focused = Some(focused);
    for _ in 0..2 {
        app.update();
    }

    let label_of = |app: &App, field: Entity| -> Entity {
        let children = app.world().get::<Children>(field).expect("label child");
        children
            .iter()
            .find(|c| app.world().get::<Text>(*c).is_some())
            .expect("label")
    };
    let tick = |app: &App, field: Entity| {
        app.world()
            .entity(label_of(app, field))
            .get_change_ticks::<Text>()
            .expect("text ticks")
            .changed
    };
    let secret_tick = tick(&app, secret_id);
    let focused_tick = tick(&app, focused);
    let before = *app.world().resource::<UiHotPathCounts>();
    const FRAMES: usize = 8;
    for _ in 0..FRAMES {
        app.update();
    }
    let after = *app.world().resource::<UiHotPathCounts>();
    assert_eq!(tick(&app, secret_id), secret_tick, "secret label was rewritten");
    assert_eq!(tick(&app, focused), focused_tick, "focused label was rewritten");
    assert_eq!(
        after.text_input_syncs,
        before.text_input_syncs,
        "sync_parts rebuilt {} label strings over {FRAMES} idle frames",
        after.text_input_syncs - before.text_input_syncs
    );
    assert_eq!(
        after.caret_map_clones,
        before.caret_map_clones,
        "focused idle frames cloned the caret map {} times",
        after.caret_map_clones - before.caret_map_clones
    );
    assert_eq!(
        after.text_input_layout_writes, before.text_input_layout_writes,
        "the cached caret layout was rewritten"
    );
}

/// A caret blink must not walk the rest of an on-screen tree. The 400 texts sit inside the
/// window, in a panel that does not clip, so an off-screen cull cannot be what skips them.
#[test]
fn a_caret_blink_does_not_rewalk_idle_text() {
    use std::time::Duration;

    use regia_app::TimeUpdateStrategy;
    use regia_app::prelude::*;
    use regia_ui::focus::Focusable;
    use regia_ui::prelude::*;
    use regia_ui::style::FlexDirection;
    use regia_ui::{BackgroundKind, TextInputPart, UiHotPathCounts, UiScales};

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
    app.world_mut().resource_mut::<UiScales>().window_size = [800.0, 600.0];

    let root = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Points(800.0), Dimension::Points(600.0)],
                ..Default::default()
            }),
        ))
        .id();
    let panel = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(160.0), Dimension::Points(160.0)],
                ..Default::default()
            }),
            ChildOf(root),
        ))
        .id();
    let mut sample = None;
    for row in 0..20 {
        for col in 0..20 {
            let entity = app
                .world_mut()
                .spawn((
                    UiNode,
                    NodeStyle::new(Style {
                        position: PositionType::Absolute,
                        inset: Edges {
                            left: Dimension::Points(col as f32 * 8.0),
                            top: Dimension::Points(row as f32 * 8.0),
                            right: Dimension::Auto,
                            bottom: Dimension::Auto,
                        },
                        size: [Dimension::Points(8.0), Dimension::Points(8.0)],
                        ..Default::default()
                    }),
                    Background {
                        kind: BackgroundKind::Color(UiColor([0.2, 0.2, 0.2, 1.0])),
                    },
                    Text::new("x"),
                    ChildOf(panel),
                ))
                .id();
            if sample.is_none() {
                sample = Some(entity);
            }
        }
    }
    let sample = sample.expect("grid");
    let mut field_text = TextInput::default();
    field_text.set_text("ab");
    let field = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(200.0), Dimension::Points(32.0)],
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            field_text,
            ChildOf(root),
        ))
        .id();
    for _ in 0..4 {
        app.update();
    }
    let rect = app.world().get::<ComputedNode>(sample).expect("laid out").rect;
    assert!(
        rect[2] > 1.0
            && rect[3] > 1.0
            && rect[0] >= 0.0
            && rect[1] >= 0.0
            && rect[0] < 800.0
            && rect[1] < 600.0,
        "the static text must be on screen so the cull cannot skip it, rect {rect:?}"
    );
    assert!(
        app.world().get::<ClipChildren>(panel).is_none(),
        "the panel must not clip"
    );
    app.world_mut()
        .resource_mut::<regia_ui::focus::FocusState>()
        .focused = Some(field);
    for _ in 0..3 {
        app.update();
    }
    let caret = app
        .world()
        .get::<Children>(field)
        .expect("field children")
        .iter()
        .find(|child| app.world().get::<TextInputPart>(*child) == Some(&TextInputPart::Caret))
        .expect("caret");
    let start = app
        .world()
        .get::<Opacity>(caret)
        .map(|opacity| opacity.0)
        .unwrap_or(1.0);
    assert!(
        (start - 1.0).abs() < 0.01,
        "caret should be in the visible half before the blink, opacity {start}"
    );
    let mut flipped = false;
    for _ in 0..8 {
        app.update();
        let now = app
            .world()
            .get::<Opacity>(caret)
            .map(|opacity| opacity.0)
            .unwrap_or(1.0);
        if (now - start).abs() > 0.01 {
            let walked = app.world().resource::<UiHotPathCounts>().extract_nodes_walked;
            assert!(
                walked < 30,
                "extract walked {walked} nodes when only the caret blinked"
            );
            flipped = true;
            break;
        }
    }
    assert!(flipped, "caret opacity stayed {start}");
}
