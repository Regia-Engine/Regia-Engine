// SPDX-License-Identifier: MPL-2.0
//! The input rebinding panel (ADR-1801, `book/agents/input-rebind.md`): a widget that lists the
//! actions of one input context with their current bindings and lets the player rebind them,
//! including key-group axes and chords, reset one action or all, and see conflicts.
//!
//! Spawn one [`RebindPanel`] node; `rebind_panels` (run by [`crate::widgets::ui_widgets`])
//! builds a row per action once, then refreshes the labels every frame from the
//! `regia_input` resources. A click on a [`RebindButton`] starts a capture
//! ([`regia_input::start_rebind`]); clicking it again while it captures cancels. The capture, the
//! override merge and the save to `user://input_overrides.ron` are `regia_input`'s
//! [`regia_input::apply_rebinding`]; this module only drives it and shows the result. Rows
//! carry [`NodeName`]s (`rebind.<context>.<action>.bind`, `.alt`, `.reset`, `.status`,
//! `rebind.<context>.reset_all`) so `ui.click` and `ui.query` address them.
//!
//! [`RebindPanelOptions`] (ADR-2129, all off by default) adds a capture button per device, dead
//! zone and invert controls for axes, and buttons that save and load named profiles.

use regia_app::prelude::*;
use regia_input::{
    ActionKind, AxisTuning, InputConfig, InputRebinder, InputSource, MAX_DEAD_ZONE, RebindMode, RebindSlot,
    axis_tuning, binding_conflicts, binding_label_in, has_dead_zone_axis, is_chord_helper, load_profile,
    reset_action, reset_all, save_profile, set_axis_tuning, start_rebind_slot,
};

use regia_ecs::ecs::message::MessageCursor;

use crate::a11y::Accessible;
use crate::focus::Focusable;
use crate::focus_graph::FocusGroup;
use crate::interaction::{TouchTarget, UiEvent, UiEventKind, UiValue};
use crate::layout::LayoutDirty;
use crate::node::{Classes, Interactable, NodeName, NodeStyle, UiNode};
use crate::style::{Align, Dimension, Edges, FlexDirection, JustifyContent, Style};
use crate::text::{Text, TextContent, TextSpan, WrapMode};
use crate::theme::Themed;
use crate::widget_style::UiDensity;
use crate::widgets::{Button, Slider, Toggle};

/// A rebinding panel for one input context. `actions` limits and orders the rows; empty lists
/// every action the context binds (alphabetical, chord helpers hidden).
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct RebindPanel {
    /// The input context whose bindings the panel edits (`Gameplay`).
    pub context: String,
    /// Which actions get a row, in order; empty = all.
    pub actions: Vec<String>,
}

/// Optional parts of a [`RebindPanel`] (ADR-2129). Put it on the panel's node before the first
/// frame; rows are built once. Without it the panel has one capture button per action, which
/// replaces all of the action's bindings.
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct RebindPanelOptions {
    /// One capture button per device (`.keyboard_mouse`, `.gamepad`): a capture replaces only that
    /// device's bindings. Replaces `.bind`.
    pub per_device: bool,
    /// Dead zone and invert controls under each axis action, one set per device with
    /// [`Self::per_device`] (`.tuning.<slot>.invert_x`, `.invert_y`, `.dead_zone`).
    pub axis_tuning: bool,
    /// Named profiles to offer: a save and a load button each
    /// (`rebind.<context>.profile.<name>.save`, `.load`).
    pub profiles: Vec<String>,
}

/// Marks a panel whose rows were built.
#[derive(Component, Debug, Clone, Copy)]
struct RebindBuilt;

/// Read position in the UI event stream, so clicks written between frames are seen once.
#[derive(Resource, Default)]
struct RebindEvents(MessageCursor<UiEvent>);

/// A button that starts (or, while it captures, cancels) a rebind of `action` in `context`.
/// The primary button uses [`RebindMode::Single`] and shows the binding; an alt button uses a
/// group mode that fits the action's kind (keys for axes, a chord for buttons).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RebindButton {
    /// The input context.
    pub context: String,
    /// The action.
    pub action: String,
    /// The capture mode this button starts.
    pub mode: RebindMode,
    /// The devices the capture listens to and replaces.
    pub slot: RebindSlot,
}

/// A button that resets one action to the project binding.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RebindReset {
    /// The input context.
    pub context: String,
    /// The action.
    pub action: String,
}

/// The button that resets every action of the panel's context ([`regia_input::reset_all`]).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RebindResetAll {
    /// The input context of the panel (used only for its name).
    pub context: String,
}

/// The status text of a row: conflicts of the last rebind, or why it was not saved.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RebindStatus {
    /// The input context.
    pub context: String,
    /// The action.
    pub action: String,
}

/// The invert switch of one axis of `action` on `slot`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RebindInvert {
    /// The input context.
    pub context: String,
    /// The action.
    pub action: String,
    /// The device slot.
    pub slot: RebindSlot,
    /// True for the y axis, false for x.
    pub y: bool,
}

/// The dead zone slider of `action` on `slot`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RebindDeadZone {
    /// The input context.
    pub context: String,
    /// The action.
    pub action: String,
    /// The device slot.
    pub slot: RebindSlot,
}

/// A button that saves the current bindings to a named profile or loads them from it.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RebindProfileButton {
    /// The profile name.
    pub profile: String,
    /// True to load, false to save.
    pub load: bool,
}

/// The text that reports the last profile save or load of a panel.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebindProfileStatus;

/// The last profile result shown by [`RebindProfileStatus`].
#[derive(Resource, Default)]
struct RebindPanelMessage(String);

/// Why the last tuning change of one action was not written to disk, for its status line.
#[derive(Resource, Default)]
struct RebindTuningError(Option<(String, String, String)>);

/// The alt mode of an action kind: a chord for buttons, key groups for axes.
fn alt_mode(kind: ActionKind) -> Option<(RebindMode, &'static str)> {
    match kind {
        ActionKind::Button => Some((RebindMode::Chord, "Chord")),
        ActionKind::Axis1D => Some((RebindMode::Keys1D, "Keys")),
        ActionKind::Axis2D | ActionKind::Axis3D => Some((RebindMode::Keys2D, "Keys")),
    }
}

fn lit_text(label: &str) -> impl Bundle {
    (
        UiNode,
        Text {
            spans: vec![TextSpan::lit(label.to_owned())],
            wrap: WrapMode::None,
            ..Default::default()
        },
    )
}

/// Widths of a per-device panel's columns, so its capture buttons line up from row to row.
const NAME_COLUMN: f32 = 96.0;
const DEVICE_COLUMN: f32 = 160.0;

/// A label at least `width` wide.
fn column_text(label: &str, width: f32) -> impl Bundle {
    (
        lit_text(label),
        NodeStyle::new(Style {
            min_size: [Dimension::Points(width), Dimension::Auto],
            ..Default::default()
        }),
    )
}

/// The density's touch target edge (`size.touch-min`: 44 UI px in the default `comfortable`).
fn touch_min(world: &World) -> f32 {
    world
        .get_resource::<UiDensity>()
        .copied()
        .unwrap_or_default()
        .touch_min()
}

fn spawn_button(world: &mut World, parent: Entity, name: &str, label: &str, extra: impl Bundle) -> Entity {
    let entity = world
        .spawn((
            UiNode,
            NodeName(name.to_owned()),
            NodeStyle::new(Style {
                padding: Edges::px(8.0),
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            TouchTarget {
                min_px: touch_min(world),
            },
            Button,
            Classes(vec!["button".to_owned()]),
            Themed::default(),
            extra,
        ))
        .id();
    let text = world.spawn(lit_text(label)).id();
    world.entity_mut(entity).add_child(text);
    world.entity_mut(parent).add_child(entity);
    entity
}

/// What a screen reader reads after a control's name: its action and, with a device, the device.
/// The row's action label and the column headers are siblings of the control, so the tree cannot
/// fold them into its name, and without this every row's "Invert Y" or "Reset" reads the same.
fn context_description(action: &str, slot: RebindSlot) -> String {
    match slot {
        RebindSlot::All => action.to_owned(),
        RebindSlot::KeyboardMouse => format!("{action}, keyboard and mouse"),
        RebindSlot::Gamepad => format!("{action}, gamepad"),
    }
}

/// The rows a panel shows: its `actions`, or every bound non-helper action of the context.
fn panel_actions(config: &InputConfig, panel: &RebindPanel) -> Vec<(String, ActionKind)> {
    let Some(ctx) = config.contexts.get(&panel.context) else {
        return Vec::new();
    };
    let bound = |action: &str| ctx.bindings.iter().any(|b| b.action == action);
    let wanted: Vec<&String> = if panel.actions.is_empty() {
        config.actions.keys().collect()
    } else {
        panel.actions.iter().collect()
    };
    wanted
        .into_iter()
        .filter(|a| !is_chord_helper(a) && bound(a))
        .filter_map(|a| config.kind(a).map(|k| (a.clone(), k)))
        .collect()
}

/// The tuning controls of `action` on `slot`, as one row under the action's row.
fn spawn_tuning_row(
    world: &mut World,
    panel_entity: Entity,
    context: &str,
    action: &str,
    kind: ActionKind,
    slot: RebindSlot,
    config: &InputConfig,
) {
    let Some(tuning) = axis_tuning(config, context, action, slot) else {
        return;
    };
    let base = format!("rebind.{context}.{action}.tuning.{}", slot.name());
    let row = world
        .spawn((
            UiNode,
            NodeName(format!("{base}.row")),
            // Left and right stay on the row while it has a control there (a pad's D-pad would
            // otherwise jump to a nearer control of the row below); up and down leave it.
            FocusGroup::new(),
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                align_items: Align::Center,
                gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                // A row as tall as a fingertip target, so the controls' touch boxes do not overlap.
                min_size: [Dimension::Auto, Dimension::Points(touch_min(world))],
                ..Default::default()
            }),
        ))
        .id();
    world.entity_mut(panel_entity).add_child(row);
    let device = match slot {
        RebindSlot::All => "",
        RebindSlot::KeyboardMouse => "Keyboard and mouse:",
        RebindSlot::Gamepad => "Gamepad:",
    };
    if !device.is_empty() {
        let text = world.spawn(column_text(device, DEVICE_COLUMN)).id();
        world.entity_mut(row).add_child(text);
    }
    let axes: &[(bool, &str, &str)] = if kind == ActionKind::Axis1D {
        &[(false, "invert_x", "Invert")]
    } else {
        &[(false, "invert_x", "Invert X"), (true, "invert_y", "Invert Y")]
    };
    for (y, suffix, text) in axes {
        let toggle = world
            .spawn((
                UiNode,
                NodeName(format!("{base}.{suffix}")),
                NodeStyle::new(Style {
                    size: [Dimension::Points(44.0), Dimension::Points(24.0)],
                    ..Default::default()
                }),
                Interactable::default(),
                Focusable::default(),
                TouchTarget {
                    min_px: touch_min(world),
                },
                Toggle {
                    checked: if *y { tuning.invert_y } else { tuning.invert_x },
                },
                // The label beside it is a sibling, so the tree cannot fold it in.
                Accessible::default()
                    .with_label(*text)
                    .with_description(context_description(action, slot)),
                Classes(vec!["toggle".to_owned()]),
                Themed::default(),
                RebindInvert {
                    context: context.to_owned(),
                    action: action.to_owned(),
                    slot,
                    y: *y,
                },
            ))
            .id();
        let text = world.spawn(lit_text(text)).id();
        world.entity_mut(row).add_child(toggle);
        world.entity_mut(row).add_child(text);
    }
    if has_dead_zone_axis(config, context, action, slot) {
        let text = world.spawn(lit_text("Dead zone")).id();
        world.entity_mut(row).add_child(text);
        let slider = world
            .spawn((
                UiNode,
                NodeName(format!("{base}.dead_zone")),
                NodeStyle::new(Style {
                    size: [Dimension::Points(160.0), Dimension::Points(28.0)],
                    // The thumb reaches past the track at its ends.
                    margin: Edges {
                        left: Dimension::Points(14.0),
                        right: Dimension::Points(14.0),
                        ..Edges::px(0.0)
                    },
                    ..Default::default()
                }),
                Interactable::default(),
                Focusable::default(),
                TouchTarget {
                    min_px: touch_min(world),
                },
                Slider {
                    value: tuning.dead_zone.unwrap_or(0.0),
                    min: 0.0,
                    max: MAX_DEAD_ZONE,
                    step: 0.05,
                },
                Accessible::default()
                    .with_label("Dead zone")
                    .with_description(context_description(action, slot)),
                Classes(vec!["slider-track".to_owned()]),
                Themed::default(),
                RebindDeadZone {
                    context: context.to_owned(),
                    action: action.to_owned(),
                    slot,
                },
            ))
            .id();
        world.entity_mut(row).add_child(slider);
    }
}

fn build_panel(
    world: &mut World,
    panel_entity: Entity,
    panel: &RebindPanel,
    options: &RebindPanelOptions,
    config: &InputConfig,
) {
    let context = panel.context.clone();
    world
        .entity_mut(panel_entity)
        .insert((RebindBuilt, NodeStyle::new(crate::widgets::panel_style())));
    if options.per_device {
        let header = world
            .spawn((
                UiNode,
                NodeName(format!("rebind.{context}.header")),
                NodeStyle::new(Style {
                    direction: FlexDirection::Row,
                    gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                    ..Default::default()
                }),
            ))
            .id();
        world.entity_mut(panel_entity).add_child(header);
        for (text, width) in [
            ("", NAME_COLUMN),
            ("Keyboard and mouse", DEVICE_COLUMN),
            ("Gamepad", DEVICE_COLUMN),
        ] {
            let text = world.spawn(column_text(text, width)).id();
            world.entity_mut(header).add_child(text);
        }
    }
    for (action, kind) in panel_actions(config, panel) {
        let base = format!("rebind.{context}.{action}");
        let row = world
            .spawn((
                UiNode,
                NodeName(format!("{base}.row")),
                // Left and right stay on the row while it has a control there (a pad's D-pad would
                // otherwise jump to a nearer control of the row below); up and down leave it.
                FocusGroup::new(),
                NodeStyle::new(Style {
                    direction: FlexDirection::Row,
                    align_items: Align::Center,
                    gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                    // A row as tall as a fingertip target, so the controls' touch boxes do not overlap.
                    min_size: [Dimension::Auto, Dimension::Points(touch_min(world))],
                    ..Default::default()
                }),
            ))
            .id();
        world.entity_mut(panel_entity).add_child(row);
        let label = if options.per_device {
            world.spawn(column_text(&action, NAME_COLUMN)).id()
        } else {
            world.spawn(lit_text(&action)).id()
        };
        world.entity_mut(row).add_child(label);
        let slots: &[(RebindSlot, &str)] = if options.per_device {
            &[
                (RebindSlot::KeyboardMouse, "keyboard_mouse"),
                (RebindSlot::Gamepad, "gamepad"),
            ]
        } else {
            &[(RebindSlot::All, "bind")]
        };
        for (slot, suffix) in slots {
            let button = spawn_button(
                world,
                row,
                &format!("{base}.{suffix}"),
                "",
                (
                    RebindButton {
                        context: context.clone(),
                        action: action.clone(),
                        mode: RebindMode::Single,
                        slot: *slot,
                    },
                    Accessible::default().with_description(context_description(&action, *slot)),
                ),
            );
            if options.per_device
                && let Some(mut node) = world.get_mut::<NodeStyle>(button)
            {
                node.0.min_size[0] = Dimension::Points(DEVICE_COLUMN);
            }
        }
        if let Some((mode, text)) = alt_mode(kind) {
            let slot = if options.per_device {
                RebindSlot::KeyboardMouse
            } else {
                RebindSlot::All
            };
            spawn_button(
                world,
                row,
                &format!("{base}.alt"),
                text,
                (
                    RebindButton {
                        context: context.clone(),
                        action: action.clone(),
                        mode,
                        slot,
                    },
                    Accessible::default().with_description(context_description(&action, slot)),
                ),
            );
            if options.per_device && mode == RebindMode::Chord {
                spawn_button(
                    world,
                    row,
                    &format!("{base}.gamepad_alt"),
                    "Pad chord",
                    (
                        RebindButton {
                            context: context.clone(),
                            action: action.clone(),
                            mode,
                            slot: RebindSlot::Gamepad,
                        },
                        Accessible::default()
                            .with_description(context_description(&action, RebindSlot::Gamepad)),
                    ),
                );
            }
        }
        spawn_button(
            world,
            row,
            &format!("{base}.reset"),
            "Reset",
            (
                RebindReset {
                    context: context.clone(),
                    action: action.clone(),
                },
                Accessible::default().with_description(action.clone()),
            ),
        );
        let status = world
            .spawn((lit_text(""), NodeName(format!("{base}.status"))))
            .id();
        world.entity_mut(status).insert(RebindStatus {
            context: context.clone(),
            action: action.clone(),
        });
        world.entity_mut(row).add_child(status);
        if options.axis_tuning && kind != ActionKind::Button {
            let slots: &[RebindSlot] = if options.per_device {
                &[RebindSlot::KeyboardMouse, RebindSlot::Gamepad]
            } else {
                &[RebindSlot::All]
            };
            for slot in slots {
                spawn_tuning_row(world, panel_entity, &context, &action, kind, *slot, config);
            }
        }
    }
    spawn_button(
        world,
        panel_entity,
        &format!("rebind.{context}.reset_all"),
        "Reset all",
        RebindResetAll {
            context: context.clone(),
        },
    );
    for profile in &options.profiles {
        let base = format!("rebind.{context}.profile.{profile}");
        let row = world
            .spawn((
                UiNode,
                NodeName(format!("{base}.row")),
                // Left and right stay on the row while it has a control there (a pad's D-pad would
                // otherwise jump to a nearer control of the row below); up and down leave it.
                FocusGroup::new(),
                NodeStyle::new(Style {
                    direction: FlexDirection::Row,
                    align_items: Align::Center,
                    gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                    // A row as tall as a fingertip target, so the controls' touch boxes do not overlap.
                    min_size: [Dimension::Auto, Dimension::Points(touch_min(world))],
                    ..Default::default()
                }),
            ))
            .id();
        world.entity_mut(panel_entity).add_child(row);
        let text = world.spawn(lit_text(&format!("Profile {profile}"))).id();
        world.entity_mut(row).add_child(text);
        for (load, suffix, text) in [(false, "save", "Save"), (true, "load", "Load")] {
            spawn_button(
                world,
                row,
                &format!("{base}.{suffix}"),
                text,
                (
                    RebindProfileButton {
                        profile: profile.clone(),
                        load,
                    },
                    Accessible::default().with_description(format!("Profile {profile}")),
                ),
            );
        }
    }
    if !options.profiles.is_empty() {
        let status = world
            .spawn((
                lit_text(""),
                NodeName(format!("rebind.{context}.profile_status")),
                RebindProfileStatus,
            ))
            .id();
        world.entity_mut(panel_entity).add_child(status);
    }
    world.resource_mut::<LayoutDirty>().mark();
}

/// Sets the first span of `entity`'s own text (or of its first text child) to `label`. Returns
/// whether the text changed.
fn set_label(world: &mut World, entity: Entity, label: &str) -> bool {
    let target = if world.get::<Text>(entity).is_some() {
        Some(entity)
    } else {
        world
            .get::<Children>(entity)
            .and_then(|c| c.iter().find(|e| world.get::<Text>(*e).is_some()))
    };
    let Some(target) = target else {
        return false;
    };
    let Some(mut text) = world.get_mut::<Text>(target) else {
        return false;
    };
    let same = matches!(
        text.spans.first().map(|s| &s.content),
        Some(TextContent::Lit(current)) if current == label
    );
    if same {
        return false;
    }
    text.spans = vec![TextSpan::lit(label.to_owned())];
    true
}

/// The label a [`RebindButton`] shows right now.
fn button_label(world: &World, button: &RebindButton) -> String {
    let rebinder = world.get_resource::<InputRebinder>();
    let capturing = rebinder.is_some_and(|r| {
        r.pending() == Some((button.context.as_str(), button.action.as_str()))
            && r.mode() == button.mode
            && r.slot() == button.slot
    });
    if capturing && let Some(progress) = rebinder.and_then(InputRebinder::progress) {
        if button.mode == RebindMode::Single {
            return match button.slot {
                RebindSlot::All => "Press an input...",
                RebindSlot::KeyboardMouse
                    if world
                        .get_resource::<InputConfig>()
                        .and_then(|c| c.kind(&button.action))
                        .is_some_and(|k| matches!(k, ActionKind::Axis2D | ActionKind::Axis3D)) =>
                {
                    "Press a key or move the mouse..."
                }
                RebindSlot::KeyboardMouse => "Press a key or mouse button...",
                RebindSlot::Gamepad => "Press a button or move a stick...",
            }
            .to_owned();
        }
        let first = progress
            .captured
            .last()
            .map_or(String::new(), |c| format!(" after {c}"));
        return format!(
            "Press {} ({}/{}){first}",
            progress.prompt,
            progress.step + 1,
            progress.total
        );
    }
    if button.mode == RebindMode::Single {
        return world
            .get_resource::<InputConfig>()
            .and_then(|c| binding_label_in(c, &button.context, &button.action, button.slot))
            .unwrap_or_else(|| "Unbound".to_owned());
    }
    match button.mode {
        RebindMode::Chord => "Chord",
        _ => "Keys",
    }
    .to_owned()
}

/// The status line of one row.
fn status_label(world: &World, status: &RebindStatus) -> String {
    if let Some(RebindTuningError(Some((context, action, err)))) = world.get_resource::<RebindTuningError>()
        && *context == status.context
        && *action == status.action
    {
        return format!("Not saved: {err}");
    }
    if let Some(rebinder) = world.get_resource::<InputRebinder>()
        && let Some(err) = &rebinder.persist_error
        && rebinder
            .outcome
            .as_ref()
            .is_some_and(|o| o.context == status.context && o.action == status.action)
    {
        return format!("Not saved: {err}");
    }
    // Read from the live bindings, so a conflict from an earlier session shows as well.
    let conflicts = world
        .get_resource::<InputConfig>()
        .map(|config| binding_conflicts(config, &status.context, &status.action))
        .unwrap_or_default();
    if conflicts.is_empty() {
        String::new()
    } else {
        format!("Also used by: {}", conflicts.join(", "))
    }
}

/// The inputs that leave a capture started from a panel: a player with one device can always back
/// out, and the capture cannot bind them. `BrowserBack` is Android's system Back, so a phone
/// player who started a capture by touch can leave it (ADR-4932).
fn panel_cancel_inputs() -> Vec<InputSource> {
    vec![
        InputSource::key("Escape"),
        InputSource::key("BrowserBack"),
        InputSource::gamepad_button(regia_platform::GamepadButton::Start),
    ]
}

/// Reacts to a tuning control's new value: writes it and saves.
fn react_value(world: &mut World, target: Entity, value: &UiValue) {
    let current = |world: &World, context: &str, action: &str, slot| {
        world
            .get_resource::<InputConfig>()
            .and_then(|config| axis_tuning(config, context, action, slot))
    };
    let (context, action, slot, tuning) = match value {
        UiValue::Bool(checked) => {
            let Some(invert) = world.get::<RebindInvert>(target).cloned() else {
                return;
            };
            let Some(mut tuning) = current(world, &invert.context, &invert.action, invert.slot) else {
                return;
            };
            if invert.y {
                tuning.invert_y = *checked;
            } else {
                tuning.invert_x = *checked;
            }
            (invert.context, invert.action, invert.slot, tuning)
        }
        UiValue::Number(number) => {
            let Some(zone) = world.get::<RebindDeadZone>(target).cloned() else {
                return;
            };
            let Some(tuning) = current(world, &zone.context, &zone.action, zone.slot) else {
                return;
            };
            let dead_zone = Some(number.clamp(0.0, MAX_DEAD_ZONE));
            // A drag repeats its value every frame; only a new step writes the file.
            if tuning.dead_zone == dead_zone {
                return;
            }
            let tuning = AxisTuning { dead_zone, ..tuning };
            (zone.context, zone.action, zone.slot, tuning)
        }
        _ => return,
    };
    if let Err(err) = set_axis_tuning(world, &context, &action, slot, tuning) {
        tracing::warn!(target: "regia_ui", "rebind panel tuning: {err}");
        return;
    }
    let not_saved = world
        .get_resource::<InputRebinder>()
        .and_then(|r| r.persist_error.clone())
        .map(|err| (context, action, err));
    world.insert_resource(RebindTuningError(not_saved));
}

/// Reacts to a click on one of the panel's buttons.
fn react(world: &mut World, event: &UiEvent) {
    if !matches!(event.kind, UiEventKind::Click) {
        return;
    }
    let target = event.target;
    if let Some(button) = world.get::<RebindButton>(target).cloned() {
        let capturing = world.get_resource::<InputRebinder>().is_some_and(|r| {
            r.pending() == Some((button.context.as_str(), button.action.as_str()))
                && r.mode() == button.mode
                && r.slot() == button.slot
        });
        if capturing {
            if let Some(mut rebinder) = world.get_resource_mut::<InputRebinder>() {
                rebinder.cancel();
            }
        } else {
            match start_rebind_slot(world, &button.context, &button.action, button.mode, button.slot) {
                Ok(()) => {
                    if let Some(mut rebinder) = world.get_resource_mut::<InputRebinder>() {
                        rebinder.set_cancel_inputs(panel_cancel_inputs());
                    }
                }
                Err(err) => tracing::warn!(target: "regia_ui", "rebind panel: {err}"),
            }
        }
    } else if let Some(reset) = world.get::<RebindReset>(target).cloned() {
        if let Err(err) = reset_action(world, &reset.context, &reset.action) {
            tracing::warn!(target: "regia_ui", "rebind panel reset: {err}");
        }
    } else if world.get::<RebindResetAll>(target).is_some() {
        if let Err(err) = reset_all(world) {
            tracing::warn!(target: "regia_ui", "rebind panel reset all: {err}");
        }
    } else if let Some(button) = world.get::<RebindProfileButton>(target).cloned() {
        let verb = if button.load { "Loaded" } else { "Saved" };
        let result = if button.load {
            load_profile(world, Some(&button.profile)).map(|_| ())
        } else {
            save_profile(world, Some(&button.profile)).map(|_| ())
        };
        let message = match result {
            Ok(()) => format!("{verb} profile {}", button.profile),
            Err(err) => err.to_string(),
        };
        world.insert_resource(RebindPanelMessage(message));
    }
}

/// Builds new panels, reacts to clicks and refreshes labels. Called from
/// [`crate::widgets::ui_widgets`] every frame; costs nothing without a [`RebindPanel`].
pub(crate) fn rebind_panels(world: &mut World) {
    let new_panels: Vec<(Entity, RebindPanel)> = world
        .query_filtered::<(Entity, &RebindPanel), Without<RebindBuilt>>()
        .iter(world)
        .map(|(e, p)| (e, p.clone()))
        .collect();
    let any_built = !new_panels.is_empty() || world.query::<&RebindBuilt>().iter(world).next().is_some();
    if !any_built {
        return;
    }
    let _span = tracing::trace_span!("ui_rebind_panels").entered();
    if let Some(config) = world.get_resource::<InputConfig>().cloned() {
        for (entity, panel) in new_panels {
            let options = world
                .get::<RebindPanelOptions>(entity)
                .cloned()
                .unwrap_or_default();
            build_panel(world, entity, &panel, &options, &config);
        }
    }
    let mut cursor = world.remove_resource::<RebindEvents>().unwrap_or_default();
    let events: Vec<UiEvent> = world
        .get_resource::<Messages<UiEvent>>()
        .map(|messages| cursor.0.read(messages).cloned().collect())
        .unwrap_or_default();
    world.insert_resource(cursor);
    // The press a capture consumed reaches the UI as well: the click on its release must not start
    // the next capture or flip whatever is under the pointer. Refreshing below undoes a toggle.
    let guarded = world
        .get_resource::<InputRebinder>()
        .is_some_and(InputRebinder::click_guard);
    if !guarded {
        for event in &events {
            match &event.kind {
                UiEventKind::ValueChanged(value) => react_value(world, event.target, value),
                _ => react(world, event),
            }
        }
    }
    let buttons: Vec<(Entity, RebindButton)> = world
        .query::<(Entity, &RebindButton)>()
        .iter(world)
        .map(|(e, b)| (e, b.clone()))
        .collect();
    let mut changed = false;
    for (entity, button) in buttons {
        let label = button_label(world, &button);
        changed |= set_label(world, entity, &label);
    }
    let statuses: Vec<(Entity, RebindStatus)> = world
        .query::<(Entity, &RebindStatus)>()
        .iter(world)
        .map(|(e, s)| (e, s.clone()))
        .collect();
    for (entity, status) in statuses {
        let label = status_label(world, &status);
        changed |= set_label(world, entity, &label);
    }
    changed |= refresh_controls(world);
    if changed {
        world.resource_mut::<LayoutDirty>().mark();
    }
}

/// Puts the tuning controls and the profile status in step with the live config: a reset, a
/// loaded profile or a refused change shows at once. Returns whether a label changed.
fn refresh_controls(world: &mut World) -> bool {
    let Some(config) = world.get_resource::<InputConfig>().cloned() else {
        return false;
    };
    let inverts: Vec<(Entity, RebindInvert)> = world
        .query::<(Entity, &RebindInvert)>()
        .iter(world)
        .map(|(e, i)| (e, i.clone()))
        .collect();
    for (entity, invert) in inverts {
        let Some(tuning) = axis_tuning(&config, &invert.context, &invert.action, invert.slot) else {
            continue;
        };
        let checked = if invert.y {
            tuning.invert_y
        } else {
            tuning.invert_x
        };
        if world.get::<Toggle>(entity).is_some_and(|t| t.checked != checked)
            && let Some(mut toggle) = world.get_mut::<Toggle>(entity)
        {
            toggle.checked = checked;
        }
    }
    let zones: Vec<(Entity, RebindDeadZone)> = world
        .query::<(Entity, &RebindDeadZone)>()
        .iter(world)
        .map(|(e, z)| (e, z.clone()))
        .collect();
    for (entity, zone) in zones {
        let Some(tuning) = axis_tuning(&config, &zone.context, &zone.action, zone.slot) else {
            continue;
        };
        let value = tuning.dead_zone.unwrap_or(0.0);
        if world.get::<Slider>(entity).is_some_and(|s| s.value != value)
            && let Some(mut slider) = world.get_mut::<Slider>(entity)
        {
            slider.value = value;
        }
    }
    let message = world
        .get_resource::<RebindPanelMessage>()
        .map(|m| m.0.clone())
        .unwrap_or_default();
    let statuses: Vec<Entity> = world
        .query_filtered::<Entity, With<RebindProfileStatus>>()
        .iter(world)
        .collect();
    let mut changed = false;
    for entity in statuses {
        changed |= set_label(world, entity, &message);
    }
    changed
}
