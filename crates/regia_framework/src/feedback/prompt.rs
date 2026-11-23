// SPDX-License-Identifier: MPL-2.0
//! The feedback prompt and the toast, built from `regia_ui` runtime widgets.
//!
//! The prompt is one panel: four category buttons (bug / performance / visual / other), one text
//! field, a line saying where the file goes and that nothing is sent, and Cancel / Send. Enter in
//! the text field sends; Escape cancels. The game keeps running underneath (no pause, networking
//! untouched).

use regia_app::prelude::*;
use regia_ui::focus::{FocusScope, FocusState, Focusable};
use regia_ui::node::{Background, BackgroundKind, Focused, Interactable, NodeStyle, UiNode, UiRoot};
use regia_ui::style::{Align, Dimension, Edges, FlexDirection, JustifyContent, Style};
use regia_ui::text::{Text, TextAlign, TextSpan, TextStyle, WrapMode};
use regia_ui::widgets::{TextInput, spawn_button};
use regia_ui::{Classes, ScaleMode, Themed, UiColor};

use super::bundle::FeedbackCategory;

/// Draw order of the prompt (above game UI and the calibration screen).
pub const PROMPT_SORT_ORDER: i32 = 2000;
/// Draw order of the toast.
pub const TOAST_SORT_ORDER: i32 = 2001;
/// Longest note, in characters.
pub const NOTE_MAX_CHARS: usize = 2000;

/// A prompt button.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptButton {
    /// Selects a category.
    Category(FeedbackCategory),
    /// Writes the bundle.
    Send,
    /// Closes without writing.
    Cancel,
}

/// The prompt's note field.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptNote;

/// A "Send feedback" entry in the game's own menu (see [`super::spawn_feedback_button`]):
/// clicking it opens the prompt.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeedbackMenuButton;

fn px(v: f32) -> Dimension {
    Dimension::Points(v)
}

fn text(world: &mut World, s: impl Into<String>, size: f32, color: UiColor, wrap: WrapMode) -> Entity {
    world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan {
                    style: TextStyle {
                        font_px: size,
                        color,
                        ..Default::default()
                    },
                    ..TextSpan::lit(s.into())
                }],
                align: TextAlign::Start,
                wrap,
                ..Default::default()
            },
        ))
        .id()
}

fn node(world: &mut World, style: Style) -> Entity {
    world.spawn((UiNode, NodeStyle::new(style))).id()
}

fn add_children(world: &mut World, parent: Entity, children: &[Entity]) {
    let mut parent = world.entity_mut(parent);
    for child in children {
        parent.add_child(*child);
    }
}

fn button(world: &mut World, parent: Entity, label: &str, primary: bool, which: PromptButton) -> Entity {
    let classes: &[&str] = if primary { &["primary"] } else { &[] };
    let mut commands = world.commands();
    let entity = spawn_button(&mut commands, parent, label, classes);
    commands.entity(entity).insert(which);
    world.flush();
    entity
}

fn note_input(note: &str) -> TextInput {
    let mut input = TextInput::default();
    input.text = note.to_owned();
    input.placeholder = "What happened? (optional; Enter sends)".to_owned();
    input.multiline = false;
    input.max_chars = NOTE_MAX_CHARS;
    input.caret = note.len();
    input.selection_anchor = note.len();
    input
}

/// Builds the prompt. Returns `(root, note field)`.
pub(crate) fn build_prompt(
    world: &mut World,
    category: FeedbackCategory,
    note: &str,
    folder: &str,
) -> (Entity, Entity) {
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                sort_order: PROMPT_SORT_ORDER,
                ..Default::default()
            },
            UiNode,
            FocusScope,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                padding: Edges::px(16.0),
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor::linear(0.0, 0.0, 0.0, 0.45)),
            },
        ))
        .id();
    let panel = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                padding: Edges::px(20.0),
                gap: [px(12.0), px(12.0)],
                size: [px(600.0), Dimension::Auto],
                max_size: [Dimension::Percent(100.0), Dimension::Auto],
                ..Default::default()
            }),
            Classes(vec!["panel".to_owned()]),
            Themed::default(),
        ))
        .id();
    let title = text(world, "Send feedback", 24.0, UiColor::WHITE, WrapMode::None);
    let categories = node(
        world,
        Style {
            direction: FlexDirection::Row,
            gap: [px(8.0), px(8.0)],
            ..Default::default()
        },
    );
    for c in FeedbackCategory::PLAYER {
        button(
            world,
            categories,
            c.label(),
            c == category,
            PromptButton::Category(c),
        );
    }
    let field = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Auto],
                min_size: [Dimension::Auto, px(96.0)],
                padding: Edges::px(8.0),
                ..Default::default()
            }),
            note_input(note),
            Focusable::default(),
            Interactable::default(),
            Classes(vec!["surface".to_owned()]),
            Themed::default(),
            PromptNote,
        ))
        .id();
    // The label child, word-wrapped (the widget would add an unwrapped one).
    let shown = if note.is_empty() {
        "What happened? (optional; Enter sends)".to_owned()
    } else {
        note.to_owned()
    };
    let label = text(world, shown, 16.0, UiColor::WHITE, WrapMode::Word);
    add_children(world, field, &[label]);
    let privacy = text(
        world,
        format!(
            "Saved on this computer only, in {folder}. A screenshot, recent frame times, your \
             settings, system info and the recent log go with your note. Nothing is sent unless \
             you share the file."
        ),
        13.0,
        UiColor::linear(0.75, 0.75, 0.75, 1.0),
        WrapMode::Word,
    );
    let buttons = node(
        world,
        Style {
            direction: FlexDirection::Row,
            justify_content: JustifyContent::End,
            gap: [px(12.0), px(12.0)],
            ..Default::default()
        },
    );
    button(world, buttons, "Cancel", false, PromptButton::Cancel);
    button(world, buttons, "Send", true, PromptButton::Send);
    add_children(world, panel, &[title, categories, field, privacy, buttons]);
    add_children(world, root, &[panel]);
    if let Some(mut focus) = world.get_resource_mut::<FocusState>() {
        focus.focused = Some(field);
    }
    world.entity_mut(field).insert(Focused);
    mark_layout(world);
    (root, field)
}

/// Builds a toast: one line of text near the top of the screen.
pub(crate) fn build_toast(world: &mut World, message: &str) -> Entity {
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                sort_order: TOAST_SORT_ORDER,
                ..Default::default()
            },
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                align_items: Align::Center,
                padding: Edges::px(24.0),
                ..Default::default()
            }),
        ))
        .id();
    let panel = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                padding: Edges::px(12.0),
                max_size: [px(900.0), Dimension::Auto],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor::linear(0.02, 0.02, 0.02, 0.85)),
            },
        ))
        .id();
    let line = text(world, message, 16.0, UiColor::WHITE, WrapMode::Word);
    add_children(world, panel, &[line]);
    add_children(world, root, &[panel]);
    mark_layout(world);
    root
}

pub(crate) fn mark_layout(world: &mut World) {
    if let Some(mut dirty) = world.get_resource_mut::<regia_ui::LayoutDirty>() {
        dirty.mark();
    }
}

/// Whether the runtime UI is installed (the prompt and toasts need it).
pub(crate) fn ui_available(world: &World) -> bool {
    world.contains_resource::<regia_ui::LayoutDirty>()
}
