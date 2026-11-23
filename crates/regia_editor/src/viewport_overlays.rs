// SPDX-License-Identifier: MPL-2.0
//! Movable viewport control groups and per-user layouts (ADR-8199).
//!
//! Areas never allocate space in the pane's parent UI. Layouts and named presets live
//! in the existing extensible preferences map, separate from frozen project state.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::editor_prefs::{EditorPreferences, PrefValue, PrefsStore};
use crate::viewport_agent::Handler;
use crate::views::ViewKind;

const PREF_KEY: &str = "viewport.overlays";
const MAX_PRESETS: usize = 32;
const MAX_SAVED_BYTES: usize = 256 * 1024;

/// A toolbar control group; widget identities use its stable key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Group {
    View,
    Transform,
    Snap,
    Camera,
    Tools,
    Select,
}

impl Group {
    const ALL: [Self; 6] = [
        Self::View,
        Self::Transform,
        Self::Snap,
        Self::Camera,
        Self::Tools,
        Self::Select,
    ];

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::View => "view_show",
            Self::Transform => "transform",
            Self::Snap => "snap",
            Self::Camera => "camera",
            Self::Tools => "tools",
            Self::Select => "select",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::View => "View / Show",
            Self::Transform => "Transform",
            Self::Snap => "Snap",
            Self::Camera => "Camera",
            Self::Tools => "Tools",
            Self::Select => "Select",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::View => "V",
            Self::Transform => "T",
            Self::Snap => "#",
            Self::Camera => "C",
            Self::Tools => "+",
            Self::Select => "S",
        }
    }

    fn width(self) -> f32 {
        match self {
            Self::View => 1000.0,
            Self::Transform => 300.0,
            Self::Snap => 420.0,
            Self::Camera => 760.0,
            Self::Tools => 120.0,
            Self::Select => 140.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Dock {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
    Floating,
}

impl Dock {
    const ALL: [Self; 5] = [Self::Top, Self::Bottom, Self::Left, Self::Right, Self::Floating];
    fn name(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::Right => "right",
            Self::Floating => "floating",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Overlay {
    dock: Dock,
    position: [f32; 2],
    collapsed: bool,
    vertical: bool,
}

type Layout = BTreeMap<String, Overlay>;

fn default_layout() -> Layout {
    Group::ALL
        .into_iter()
        .map(|g| (g.key().to_owned(), Overlay::default()))
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Saved {
    panes: BTreeMap<String, Layout>,
    presets: BTreeMap<String, BTreeMap<String, Layout>>,
}

fn error(code: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({"code":code})),
    }
}

fn invalid(message: impl Into<String>) -> RpcError {
    error("viewport_overlay_invalid", message)
}

fn valid_layout(layout: &Layout) -> bool {
    layout.len() == Group::ALL.len()
        && Group::ALL.iter().all(|g| {
            layout.get(g.key()).is_some_and(|s| {
                s.position
                    .iter()
                    .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
            })
        })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.trim() == name && !name.chars().any(char::is_control)
}

#[derive(Resource)]
struct Cache {
    generation: u64,
    saved: Result<Arc<Saved>, RpcError>,
}

fn read(world: &mut World) -> Result<Arc<Saved>, RpcError> {
    crate::editor_prefs::load_prefs(world);
    let prefs = world.resource::<EditorPreferences>();
    let generation = prefs.generation();
    if let Some(cache) = world.get_resource::<Cache>()
        && cache.generation == generation
    {
        return cache.saved.clone();
    }
    let saved = parse(prefs.extra(PREF_KEY)).map(Arc::new);
    world.insert_resource(Cache {
        generation,
        saved: saved.clone(),
    });
    saved
}

fn parse(value: Option<&PrefValue>) -> Result<Saved, RpcError> {
    let Some(value) = value else {
        return Ok(Saved::default());
    };
    let PrefValue::Text(text) = value else {
        return Err(error(
            "viewport_overlay_saved_invalid",
            "Saved overlay preferences are not a string; reset to recover",
        ));
    };
    let saved: Saved = if text.len() <= MAX_SAVED_BYTES {
        serde_json::from_str(text).map_err(|_| {
            error(
                "viewport_overlay_saved_invalid",
                "Saved overlay preferences cannot be read; reset to recover",
            )
        })?
    } else {
        return Err(error(
            "viewport_overlay_saved_invalid",
            "Saved overlay preferences exceed the size limit; reset to recover",
        ));
    };
    let valid = saved
        .panes
        .iter()
        .all(|(pane, layout)| ViewKind::from_name(pane).is_some() && valid_layout(layout))
        && saved.presets.iter().all(|(pane, presets)| {
            ViewKind::from_name(pane).is_some()
                && presets.len() <= MAX_PRESETS
                && presets
                    .iter()
                    .all(|(name, layout)| valid_name(name) && valid_layout(layout))
        });
    if !valid {
        return Err(error(
            "viewport_overlay_saved_invalid",
            "Saved overlay preferences are invalid; reset to recover",
        ));
    }
    Ok(saved)
}

fn write(world: &mut World, saved: &Saved) -> Result<(), RpcError> {
    if world
        .get_resource::<PrefsStore>()
        .is_some_and(|p| p.read_only.is_some())
    {
        return Err(error(
            "viewport_overlay_read_only",
            "The preferences file is from a newer editor and cannot be changed",
        ));
    }
    let text = serde_json::to_string(saved).map_err(|e| invalid(e.to_string()))?;
    if text.len() > MAX_SAVED_BYTES {
        return Err(invalid("Overlay preferences exceed the size limit"));
    }
    world
        .get_resource_or_insert_with(EditorPreferences::default)
        .set_extra(PREF_KEY, Some(PrefValue::Text(text)));
    Ok(())
}

fn string<'a>(params: &'a Value, key: &str, default: &'a str) -> Result<&'a str, RpcError> {
    match params.get(key) {
        None => Ok(default),
        Some(Value::String(s)) => Ok(s),
        _ => Err(invalid(format!("{key} must be a string"))),
    }
}

fn apply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let object = params.as_object().ok_or_else(|| invalid("Expected an object"))?;
    const KEYS: &[&str] = &[
        "action",
        "pane",
        "group",
        "dock",
        "position",
        "collapsed",
        "vertical",
        "name",
        "selection",
    ];
    if let Some(key) = object.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(invalid(format!("Unknown overlay parameter {key}")));
    }
    let action = string(params, "action", "get")?;
    let pane = string(params, "pane", "perspective")?;
    if ViewKind::from_name(pane).is_none() {
        return Err(invalid("Unknown pane kind"));
    }
    let mut saved = match read(world) {
        Ok(saved) => (*saved).clone(),
        Err(_) if action == "reset" => Saved::default(),
        Err(error) => return Err(error),
    };
    let before = saved.clone();
    let mut layout = saved.panes.get(pane).cloned().unwrap_or_else(default_layout);
    let mut selection = None;
    match action {
        "get" => {
            if object
                .keys()
                .any(|key| !["action", "pane"].contains(&key.as_str()))
            {
                return Err(invalid("get accepts only pane"));
            }
        }
        "set" => {
            if params.get("name").is_some() {
                return Err(invalid("set does not accept name"));
            }
            let has_layout = ["dock", "position", "collapsed", "vertical"]
                .iter()
                .any(|k| params.get(k).is_some());
            if has_layout || params.get("group").is_some() {
                let group = string(params, "group", "")?;
                let overlay = layout
                    .get_mut(group)
                    .ok_or_else(|| invalid("Unknown or missing overlay group"))?;
                if let Some(dock) = params.get("dock") {
                    overlay.dock = Dock::ALL
                        .into_iter()
                        .find(|d| Some(d.name()) == dock.as_str())
                        .ok_or_else(|| invalid("dock must be top, bottom, left, right or floating"))?;
                }
                if let Some(position) = params.get("position") {
                    let position: [f32; 2] = serde_json::from_value(position.clone())
                        .map_err(|_| invalid("position must be [x, y] in 0..1"))?;
                    if !position.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)) {
                        return Err(invalid("position must be finite and in 0..1"));
                    }
                    overlay.position = position;
                }
                for (key, value) in [
                    ("collapsed", &mut overlay.collapsed),
                    ("vertical", &mut overlay.vertical),
                ] {
                    if let Some(raw) = params.get(key) {
                        *value = raw
                            .as_bool()
                            .ok_or_else(|| invalid(format!("{key} must be a boolean")))?;
                    }
                }
            }
            if let Some(shape) = params.get("selection") {
                selection = Some(match shape.as_str() {
                    Some("box") => false,
                    Some("lasso") => true,
                    _ => return Err(invalid("selection must be box or lasso")),
                });
            }
            saved.panes.insert(pane.to_owned(), layout.clone());
        }
        "reset" => {
            if object
                .keys()
                .any(|key| !["action", "pane", "group"].contains(&key.as_str()))
            {
                return Err(invalid("reset accepts pane and optional group"));
            }
            if params.get("group").is_some() {
                let group = string(params, "group", "")?;
                *layout
                    .get_mut(group)
                    .ok_or_else(|| invalid("Unknown overlay group"))? = Overlay::default();
            } else {
                layout = default_layout();
            }
            saved.panes.insert(pane.to_owned(), layout.clone());
        }
        "preset_save" | "preset_apply" | "preset_delete" => {
            if object
                .keys()
                .any(|key| !["action", "pane", "name"].contains(&key.as_str()))
            {
                return Err(invalid("Preset actions accept pane and name"));
            }
            let name = string(params, "name", "")?;
            if !valid_name(name) {
                return Err(invalid(
                    "Preset names must be 1..64 bytes, trimmed, with no control characters",
                ));
            }
            let presets = saved.presets.entry(pane.to_owned()).or_default();
            match action {
                "preset_save" => {
                    if presets.len() >= MAX_PRESETS && !presets.contains_key(name) {
                        return Err(invalid("At most 32 presets per pane kind"));
                    }
                    presets.insert(name.to_owned(), layout.clone());
                }
                "preset_apply" => {
                    layout = presets.get(name).cloned().ok_or_else(|| {
                        error(
                            "viewport_overlay_preset_missing",
                            "No overlay preset has that name",
                        )
                    })?;
                    saved.panes.insert(pane.to_owned(), layout.clone());
                }
                _ => {
                    if presets.remove(name).is_none() {
                        return Err(error(
                            "viewport_overlay_preset_missing",
                            "No overlay preset has that name",
                        ));
                    }
                }
            }
        }
        _ => return Err(invalid("Unknown overlay action")),
    }
    if saved != before || action == "reset" {
        write(world, &saved)?;
    }
    if let Some(lasso) = selection {
        world.insert_resource(crate::selection::SelectionShape { lasso });
    }
    let presets: Vec<&str> = saved
        .presets
        .get(pane)
        .map(|p| p.keys().map(String::as_str).collect())
        .unwrap_or_default();
    Ok(json!({"pane":pane, "groups":layout, "presets":presets,
        "selection":if crate::selection::SelectionShape::lasso_on(world) { "lasso" } else { "box" }}))
}

/// The overlay agent surface, with the same operations as its keyboard menu.
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, Handler)> {
    vec![(regia_agent::schema::VIEWPORT_OVERLAYS, apply as Handler)]
}

fn group_controls(
    ui: &mut egui::Ui,
    world: &mut World,
    group: Group,
    pane: &str,
    pending: &mut Vec<Value>,
    controls: &mut impl FnMut(&mut egui::Ui, &mut World, Group),
) {
    if group == Group::Select {
        let mut lasso = crate::selection::SelectionShape::lasso_on(world);
        let before = lasso;
        ui.selectable_value(&mut lasso, false, "Box");
        ui.selectable_value(&mut lasso, true, "Lasso");
        if lasso != before {
            pending.push(json!({"action":"set", "pane":pane, "selection":if lasso {"lasso"} else {"box"}}));
        }
    } else {
        controls(ui, world, group);
    }
}

fn layout_controls(ui: &mut egui::Ui, pane: &str, group: Group, state: &Overlay, pending: &mut Vec<Value>) {
    let mut next = state.clone();
    ui.checkbox(&mut next.collapsed, "Collapse to icons");
    ui.checkbox(&mut next.vertical, "Vertical");
    egui::ComboBox::from_id_salt("overlay-edge")
        .selected_text(format!("Dock: {}", next.dock.name()))
        .show_ui(ui, |ui| {
            for edge in Dock::ALL {
                ui.selectable_value(&mut next.dock, edge, edge.name());
            }
        });
    for (axis, label) in [
        (0, "Overlay horizontal position"),
        (1, "Overlay vertical position"),
    ] {
        ui.add(egui::Slider::new(&mut next.position[axis], 0.0..=1.0).text(label));
    }
    if &next != state {
        pending.push(
            json!({"action":"set", "pane":pane, "group":group.key(), "dock":next.dock,
            "position":next.position, "collapsed":next.collapsed, "vertical":next.vertical}),
        );
    }
    if ui.button("Reset overlay").clicked() {
        pending.push(json!({"action":"reset", "pane":pane, "group":group.key()}));
    }
}

// Keep the parent open independently of the combo-box popup inside it. A root menu
// backed by egui's single popup-memory slot would disappear when a combo opens.
fn overlay_menu(ui: &mut egui::Ui, label: &str, contents: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    // One line: a label never breaks inside its own button (the brand's wider font broke
    // "Overlays" in two at the pane's corner, ADR-8679).
    let response = ui.add(egui::Button::new(label).wrap_mode(egui::TextWrapMode::Extend));
    let open_id = response.id.with("overlay-menu-open");
    let frame_id = response.id.with("overlay-menu-frame");
    let frame = ui.ctx().cumulative_frame_nr();
    let mut open = ui.ctx().data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
    if response.clicked() && ui.ctx().data(|d| d.get_temp::<u64>(frame_id)) != Some(frame) {
        open = !open;
        ui.ctx().data_mut(|d| d.insert_temp(frame_id, frame));
    }
    egui::Popup::menu(&response)
        .open_bool(&mut open)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(contents);
    ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
    response
}

fn normalized_position(rect: egui::Rect, size: egui::Vec2, pos: egui::Pos2) -> [f32; 2] {
    let free = (rect.size() - size).max(egui::Vec2::splat(1.0));
    let relative = (pos - rect.min) / free;
    [relative.x.clamp(0.0, 1.0), relative.y.clamp(0.0, 1.0)]
}

fn position(rect: egui::Rect, size: egui::Vec2, state: &Overlay) -> egui::Pos2 {
    let free = (rect.size() - size).max(egui::Vec2::ZERO);
    let mut pos = rect.min + free * egui::vec2(state.position[0], state.position[1]);
    match state.dock {
        Dock::Top => pos.y = rect.min.y,
        Dock::Bottom => pos.y = rect.min.y + free.y,
        Dock::Left => pos.x = rect.min.x,
        Dock::Right => pos.x = rect.min.x + free.x,
        Dock::Floating => {}
    }
    pos
}

/// The egui temp-data key of the origin of a secondary or middle press that landed on an
/// overlay group.
fn camera_press_id() -> egui::Id {
    egui::Id::new("viewport-overlays-camera-press")
}

/// Whether a secondary or middle drag that began on an overlay group inside `pane_rect` is
/// held. The press went to the group's area (egui hit-tests it against the previous pass, where
/// the group was drawn), so the pane's own response never reports the drag; the pane camera
/// then reads the buttons and the pointer motion from the input. Forgets the press once both
/// buttons are up.
pub(crate) fn camera_drag_through(ctx: &egui::Context, pane_rect: egui::Rect) -> bool {
    let held = ctx.input(|input| {
        input.pointer.button_down(egui::PointerButton::Secondary)
            || input.pointer.button_down(egui::PointerButton::Middle)
    });
    let origin = ctx.data(|d| d.get_temp::<egui::Pos2>(camera_press_id()));
    if !held {
        if origin.is_some() {
            ctx.data_mut(|d| d.remove::<egui::Pos2>(camera_press_id()));
        }
        return false;
    }
    origin.is_some_and(|origin| pane_rect.contains(origin))
}

/// Draws independent groups over `pane_rect`; never advances `ui`'s cursor.
#[tracing::instrument(skip_all, name = "viewport_overlays")]
pub(crate) fn show(
    ui: &mut egui::Ui,
    world: &mut World,
    instance: Option<u64>,
    kind: ViewKind,
    pane_rect: egui::Rect,
    mut controls: impl FnMut(&mut egui::Ui, &mut World, Group),
) {
    let pane = kind.name();
    let id = ui.id().with(("viewport-overlays", instance, pane));
    let ctx = ui.ctx().clone();
    let frame = ctx.cumulative_frame_nr();
    let frame_id = id.with("applied-frame");
    let first_pass = ctx.data(|d| d.get_temp::<u64>(frame_id)) != Some(frame);
    let saved = read(world);
    let saved_error = saved.as_ref().err().map(|e| e.message.clone());
    let saved = saved.unwrap_or_default();
    let layout = saved.panes.get(pane).cloned().unwrap_or_else(default_layout);
    let mut pending = Vec::new();
    let margin = ui.spacing().item_spacing.x;
    let rect = pane_rect.shrink(margin);
    if rect.width() < 20.0 || rect.height() < 20.0 {
        return;
    }
    // Orbit and pan are secondary and middle drags on the pane image, which is
    // already drawn under these foreground areas. While either button is down the
    // chrome stays out of the hit test so the drag reaches the camera (a small
    // instance pane is otherwise covered by the expanded groups).
    //
    // egui hit-tests a press against the previous pass's widgets, where a group still
    // covered the pane, so the pane never becomes the drag's widget. A press that lands
    // on a group is recorded here and the pane camera takes the pointer motion itself
    // ([`camera_drag_through`]).
    let press_on_group = ctx
        .input(|input| {
            let pointer = &input.pointer;
            (pointer.button_pressed(egui::PointerButton::Secondary)
                || pointer.button_pressed(egui::PointerButton::Middle))
            .then(|| pointer.press_origin())
            .flatten()
        })
        .filter(|origin| {
            pane_rect.contains(*origin)
                && ctx
                    .layer_id_at(*origin)
                    .is_some_and(|layer| Group::ALL.iter().any(|group| layer.id == id.with(group.key())))
        });
    if let Some(origin) = press_on_group {
        ctx.data_mut(|d| d.insert_temp(camera_press_id(), origin));
    }
    let camera_gesture = ctx.input(|input| {
        let pointer = &input.pointer;
        pointer.button_down(egui::PointerButton::Secondary)
            || pointer.button_released(egui::PointerButton::Secondary)
            || pointer.button_down(egui::PointerButton::Middle)
            || pointer.button_released(egui::PointerButton::Middle)
    });
    if camera_gesture {
        return;
    }
    // Group widths are points at egui's stock text size; larger text widens them so a control
    // still fits its group (text scale 2: "Pivot: Median point" in Transform, ADR-8679).
    let text_scale = {
        let body = |styles: &std::collections::BTreeMap<egui::TextStyle, egui::FontId>| {
            styles.get(&egui::TextStyle::Body).map_or(0.0, |font| font.size)
        };
        let stock = body(&egui::style::default_text_styles());
        if stock > 0.0 {
            (body(&ui.style().text_styles) / stock).max(1.0)
        } else {
            1.0
        }
    };
    let mut flow = rect.min;
    let mut row_height = 0.0_f32;
    for group in Group::ALL {
        let Some(state) = layout.get(group.key()) else {
            continue;
        };
        let area_id = id.with(group.key());
        let width = text_scale
            * if state.collapsed {
                72.0
            } else if state.vertical {
                240.0
            } else {
                group.width()
            };
        let width = width.min(rect.width()).max(20.0);
        let size_id = area_id.with("measured-size");
        let measured = ctx.data(|d| d.get_temp::<egui::Vec2>(size_id));
        let size = measured.unwrap_or(egui::vec2(width, ui.spacing().interact_size.y + 16.0));
        let size = size.min(rect.size());
        let mut pos = position(rect, size, state);
        // Default top-edge groups pack into rows. Customized positions remain independent.
        if state.dock == Dock::Top && state.position == [0.0, 0.0] {
            if flow.x > rect.min.x && flow.x + size.x > rect.max.x {
                flow.x = rect.min.x;
                flow.y += row_height + margin;
                row_height = 0.0;
            }
            pos = flow;
            flow.x += size.x + margin;
            row_height = row_height.max(size.y);
        }
        let free = (rect.size() - size).max(egui::Vec2::ZERO);
        pos.x = pos.x.clamp(rect.min.x, rect.min.x + free.x);
        pos.y = pos.y.clamp(rect.min.y, rect.min.y + free.y);
        let shown = egui::Area::new(area_id)
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .default_size(egui::vec2(width, 32.0))
            .constrain_to(rect)
            .movable(false)
            .fade_in(false)
            .show(&ctx, |ui| {
                ui.set_width(width);
                ui.set_clip_rect(rect);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width((width - 16.0).max(4.0));
                    ui.horizontal(|ui| {
                        // The grip is outside the scroll area: dragging it must move the group,
                        // never scroll the group's content.
                        let drag = ui
                            .add(egui::Label::new("::").sense(egui::Sense::drag()))
                            .on_hover_text(format!(
                                "Drag {} overlay; keyboard: Overlays → {} → Layout",
                                group.label(),
                                group.label()
                            ));
                        crate::a11y_bridge::name_node(&drag, &format!("Drag {} overlay", group.label()));
                        if drag.dragged() && first_pass {
                            pending.push(
                                json!({"action":"set", "pane":pane, "group":group.key(), "dock":"floating",
                                "position":normalized_position(rect, size, pos + drag.drag_delta())}),
                            );
                        }
                        let menu = overlay_menu(ui, group.glyph(), |ui| {
                            ui.menu_button("Layout", |ui| {
                                layout_controls(ui, pane, group, state, &mut pending)
                            });
                            ui.separator();
                            egui::ScrollArea::both()
                                .max_height(rect.height() * 0.7)
                                .show(ui, |ui| {
                                    group_controls(ui, world, group, pane, &mut pending, &mut controls);
                                });
                        });
                        crate::a11y_bridge::name_node(&menu, &format!("{} overlay", group.label()));
                        menu.on_hover_text(format!("{} overlay", group.label()));
                    });
                    // A scroll area must be a separate vertical child: inside a wrapped
                    // header row its painted controls can extend beyond the frame response.
                    if !state.collapsed {
                        // The area's content is laid out in last frame's size, so the scroll area
                        // would never grow past it (egui falls back to its 64-point minimum): it
                        // grows to its content up to the cap instead, and wrapped rows of larger
                        // or wider text are not cut off (ADR-8679).
                        let max_height = (rect.height() - 72.0).max(16.0);
                        egui::ScrollArea::both()
                            .id_salt("overlay-content")
                            .max_width(ui.available_width().max(20.0))
                            .max_height(max_height)
                            .min_scrolled_height(max_height)
                            .show(ui, |ui| {
                                let draw = |ui: &mut egui::Ui| {
                                    group_controls(ui, world, group, pane, &mut pending, &mut controls)
                                };
                                if state.vertical {
                                    ui.vertical(draw);
                                } else {
                                    ui.horizontal_wrapped(draw);
                                }
                            });
                    }
                });
            });
        if measured.is_none() {
            // Areas measure invisibly on their first pass. Redraw in this frame so the
            // toolbar is visible and usable immediately, including after a view-kind switch.
            ctx.request_discard("Measure viewport overlays");
        }
        ctx.data_mut(|d| d.insert_temp(size_id, shown.response.rect.size()));
    }
    // Always present, including collapsed and off-edge layouts. It is on a separate layer
    // drawn last so other groups cannot cover the keyboard recovery menu.
    // In the pane's bottom-right corner by its measured size (90 x 30 points until measured),
    // so a wider font or a larger text scale keeps it whole inside the pane (ADR-8679).
    let menu_size_id = id.with("menu-size");
    let menu_size = ctx
        .data(|d| d.get_temp::<egui::Vec2>(menu_size_id))
        .unwrap_or(egui::vec2(90.0, 30.0))
        .min(rect.size());
    let menu = egui::Area::new(id.with("menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.max - menu_size)
        .constrain_to(rect)
        .movable(false)
        .fade_in(false)
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                overlay_menu(ui, "Overlays", |ui| {
                    egui::ScrollArea::both()
                        .max_height((rect.height() - 20.0).max(100.0))
                        .max_width(rect.width().max(160.0))
                        .show(ui, |ui| {
                            if let Some(error) = &saved_error {
                                ui.label(error);
                            }
                            for group in Group::ALL {
                                ui.menu_button(group.label(), |ui| {
                                    if let Some(state) = layout.get(group.key()) {
                                        ui.menu_button("Layout", |ui| {
                                            layout_controls(ui, pane, group, state, &mut pending)
                                        });
                                    }
                                    ui.separator();
                                    egui::ScrollArea::both()
                                        .max_height(rect.height() * 0.7)
                                        .show(ui, |ui| {
                                            group_controls(
                                                ui,
                                                world,
                                                group,
                                                pane,
                                                &mut pending,
                                                &mut controls,
                                            );
                                        });
                                });
                            }
                            ui.separator();
                            let name_id = id.with("preset-name");
                            let mut name = ctx.data(|d| d.get_temp::<String>(name_id)).unwrap_or_default();
                            let response = ui.text_edit_singleline(&mut name);
                            crate::a11y_bridge::name_node(&response, "Overlay preset name");
                            ctx.data_mut(|d| d.insert_temp(name_id, name.clone()));
                            if ui
                                .add_enabled(valid_name(&name), egui::Button::new("Save overlay preset"))
                                .clicked()
                            {
                                pending.push(json!({"action":"preset_save", "pane":pane, "name":name}));
                            }
                            if let Some(presets) = saved.presets.get(pane) {
                                for name in presets.keys() {
                                    ui.menu_button(name, |ui| {
                                        if ui.button("Apply overlay preset").clicked() {
                                            pending.push(
                                                json!({"action":"preset_apply", "pane":pane, "name":name}),
                                            );
                                        }
                                        if ui.button("Delete overlay preset").clicked() {
                                            pending.push(
                                                json!({"action":"preset_delete", "pane":pane, "name":name}),
                                            );
                                        }
                                    });
                                }
                            }
                            if ui.button("Reset overlay layout").clicked() {
                                pending.push(json!({"action":"reset", "pane":pane}));
                            }
                            if let Some(message) = ctx.data(|d| d.get_temp::<String>(id.with("error"))) {
                                ui.label(message);
                            }
                        });
                });
            });
        });
    let measured_menu = menu.response.rect.size();
    if ctx.data(|d| d.get_temp::<egui::Vec2>(menu_size_id)) != Some(measured_menu) {
        ctx.data_mut(|d| d.insert_temp(menu_size_id, measured_menu));
        ctx.request_discard("Measure the viewport overlays menu");
    }
    if first_pass && !pending.is_empty() {
        for params in pending {
            match apply(world, &params) {
                Ok(_) => {
                    ctx.data_mut(|d| d.remove::<String>(id.with("error")));
                }
                Err(error) => {
                    tracing::warn!(target: "regia_editor", "overlay operation: {}", error.message);
                    ctx.data_mut(|d| d.insert_temp(id.with("error"), error.message));
                }
            }
        }
        ctx.data_mut(|d| d.insert_temp(frame_id, frame));
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_preferences_reuse_the_parsed_layout() {
        let root = regia_test_utils::unique_temp_dir("overlay-cache");
        let mut world = World::new();
        world.insert_resource(crate::editor_prefs::EditorConfigPaths::at(&root));
        let first = read(&mut world).expect("defaults");
        let second = read(&mut world).expect("cached defaults");
        assert!(Arc::ptr_eq(&first, &second));
        apply(
            &mut world,
            &json!({"action":"set", "group":"snap", "collapsed":true}),
        )
        .expect("set");
        let changed = read(&mut world).expect("changed");
        assert!(!Arc::ptr_eq(&first, &changed));
        assert!(changed.panes["perspective"]["snap"].collapsed);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn edge_positions_and_normalized_drags_remain_inside_the_pane() {
        let rect = egui::Rect::from_min_size(egui::pos2(40.0, 70.0), egui::vec2(300.0, 200.0));
        for size in [egui::vec2(100.0, 50.0), rect.size()] {
            for dock in Dock::ALL {
                for fraction in [0.0, 0.3, 1.0] {
                    let state = Overlay {
                        dock,
                        position: [fraction, fraction],
                        ..Default::default()
                    };
                    let pos = position(rect, size, &state);
                    assert!(rect.contains_rect(egui::Rect::from_min_size(pos, size)));
                    let normalized = normalized_position(rect, size, pos);
                    assert!(
                        normalized
                            .iter()
                            .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
                    );
                    let floating = Overlay {
                        dock: Dock::Floating,
                        position: normalized,
                        ..Default::default()
                    };
                    assert!(position(rect, size, &floating).distance(pos) < 0.001);
                }
            }
        }
        assert_eq!(
            normalized_position(rect, rect.size(), rect.min - egui::vec2(10.0, 10.0)),
            [0.0, 0.0]
        );
    }
}
