// SPDX-License-Identifier: MPL-2.0
//! Inline curve and gradient fields (spec addendum § 7.12, ADR-7786).
//!
//! No curve or gradient type of the engine is reflected today (`regia_core::math::Curve<T>`,
//! `regia_animation::curve::Curve`, `regia_framework::curve_table::CurveKey`, the effect
//! format's `FloatKey`/`ColorKey` are serde-only), so a component field cannot hold one. The
//! drawer recognizes a key list by its shape instead: a `Vec` of a reflected struct with an `f32`
//! time field (`t`, `time`, `x` or `location`) and
//!
//! * an `f32` value field (`v`, `value` or `y`), optionally `f32` `in_tangent` and `out_tangent`:
//!   a **curve**;
//! * a `color` field (`[f32; 3]`, `[f32; 4]` linear, or a `String` hex color): a **gradient**.
//!
//! The field draws a mini graph (a curve's line, a gradient's strip). Clicking it, or Enter when
//! it has keyboard focus, opens the editor under it: for a curve, the animation curve canvas
//! (`crate::anim::curve_view`, read through this module's adapter: drag keys, double-click to
//! add, scroll to zoom, so values beyond 0-1 fit), and for both, a key table (time, value or
//! color, tangents) with **Add key**, **Remove** and presets: every key is a focusable field.
//!
//! The agent surface is `editor.curve_field_edit` ([`super::picker_agent`]).

use std::any::TypeId;

use regia_animation::curve::{Curve, CurveKey, CurveKeyKind};
use regia_ecs::reflect::list::List;
use regia_ecs::reflect::{PartialReflect, ReflectMut, ReflectRef, TypeInfo, TypeRegistry};

use super::reflect_ui::{InspectorContext, clone_value, default_value};

/// Names a time field may have.
pub const TIME_NAMES: [&str; 4] = ["t", "time", "x", "location"];
/// Names a curve's value field may have.
pub const VALUE_NAMES: [&str; 3] = ["v", "value", "y"];

/// How a gradient key stores its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorStorage {
    /// `[f32; 3]`, linear.
    Rgb,
    /// `[f32; 4]`, linear.
    Rgba,
    /// A `#rrggbb[aa]` string.
    Hex,
}

/// What kind of key list a field is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    /// A curve: the value field's name, and whether keys carry tangents.
    Curve {
        /// The value field.
        value: &'static str,
        /// Whether the key has `in_tangent` and `out_tangent`.
        tangents: bool,
    },
    /// A gradient: how the `color` field stores it.
    Gradient(ColorStorage),
}

/// A recognized key list: the key type, its time field and its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyShape {
    /// The key struct's type.
    pub item: TypeId,
    /// The time field.
    pub time: &'static str,
    /// Curve or gradient.
    pub kind: ShapeKind,
}

/// One curve key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurvePoint {
    /// Time.
    pub time: f32,
    /// Value.
    pub value: f32,
    /// `(in, out)` slopes when the key type has them.
    pub tangents: Option<(f32, f32)>,
}

/// One gradient key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradientStop {
    /// Time.
    pub time: f32,
    /// Linear RGBA color.
    pub color: [f32; 4],
}

/// The keys of a curve or gradient field.
#[derive(Debug, Clone, PartialEq)]
pub enum Keys {
    /// Curve keys.
    Curve(Vec<CurvePoint>),
    /// Gradient stops.
    Gradient(Vec<GradientStop>),
}

/// The shape of a list whose items have type info `item`, when it is a curve or a gradient.
#[must_use]
pub fn shape_of_item(registry: &TypeRegistry, item: TypeId) -> Option<KeyShape> {
    let TypeInfo::Struct(info) = registry.get(item)?.type_info() else {
        return None;
    };
    let is = |name: &str, ty: TypeId| info.field(name).is_some_and(|f| f.type_id() == ty);
    let f32_id = TypeId::of::<f32>();
    let time = TIME_NAMES.into_iter().find(|n| is(n, f32_id))?;
    if let Some(value) = VALUE_NAMES.into_iter().find(|n| is(n, f32_id)) {
        let tangents = is("in_tangent", f32_id) && is("out_tangent", f32_id);
        return Some(KeyShape {
            item,
            time,
            kind: ShapeKind::Curve { value, tangents },
        });
    }
    let storage = if is("color", TypeId::of::<[f32; 4]>()) {
        ColorStorage::Rgba
    } else if is("color", TypeId::of::<[f32; 3]>()) {
        ColorStorage::Rgb
    } else if is("color", TypeId::of::<String>()) {
        ColorStorage::Hex
    } else {
        return None;
    };
    Some(KeyShape {
        item,
        time,
        kind: ShapeKind::Gradient(storage),
    })
}

/// The shape of `value` when it is a list of curve or gradient keys.
#[must_use]
pub fn shape_of(registry: &TypeRegistry, value: &dyn PartialReflect) -> Option<KeyShape> {
    let ReflectRef::List(list) = value.reflect_ref() else {
        return None;
    };
    let item = match list.get_represented_type_info() {
        Some(TypeInfo::List(info)) => info.item_ty().id(),
        _ => list.get(0)?.get_represented_type_info()?.type_id(),
    };
    shape_of_item(registry, item)
}

fn f32_field(item: &dyn PartialReflect, name: &str) -> f32 {
    match item.reflect_ref() {
        ReflectRef::Struct(s) => s
            .field(name)
            .and_then(|f| f.try_downcast_ref::<f32>().copied())
            .unwrap_or(0.0),
        _ => 0.0,
    }
}

fn color_field(item: &dyn PartialReflect, storage: ColorStorage) -> [f32; 4] {
    let ReflectRef::Struct(s) = item.reflect_ref() else {
        return [1.0; 4];
    };
    let Some(field) = s.field("color") else {
        return [1.0; 4];
    };
    // A dynamic clone (an agent edit reads one) holds the array as a `DynamicArray`: read it by
    // element.
    let channels = || -> Option<Vec<f32>> {
        match field.reflect_ref() {
            ReflectRef::Array(array) => array
                .iter()
                .map(|c| c.try_downcast_ref::<f32>().copied())
                .collect(),
            _ => None,
        }
    };
    match storage {
        ColorStorage::Rgba => channels().and_then(|c| <[f32; 4]>::try_from(c).ok()),
        ColorStorage::Rgb => channels()
            .and_then(|c| <[f32; 3]>::try_from(c).ok())
            .map(|c| [c[0], c[1], c[2], 1.0]),
        ColorStorage::Hex => field
            .try_downcast_ref::<String>()
            .and_then(|hex| super::color::parse_hex(hex)),
    }
    .unwrap_or([1.0; 4])
}

/// Reads the keys of `list` (a list of `shape`).
#[must_use]
pub fn read_keys(list: &dyn List, shape: &KeyShape) -> Keys {
    match shape.kind {
        ShapeKind::Curve { value, tangents } => Keys::Curve(
            list.iter()
                .map(|item| CurvePoint {
                    time: f32_field(item, shape.time),
                    value: f32_field(item, value),
                    tangents: tangents
                        .then(|| (f32_field(item, "in_tangent"), f32_field(item, "out_tangent"))),
                })
                .collect(),
        ),
        ShapeKind::Gradient(storage) => Keys::Gradient(
            list.iter()
                .map(|item| GradientStop {
                    time: f32_field(item, shape.time),
                    color: color_field(item, storage),
                })
                .collect(),
        ),
    }
}

fn set_field(item: &mut dyn PartialReflect, name: &str, value: &dyn PartialReflect) -> Result<(), String> {
    let ReflectMut::Struct(s) = item.reflect_mut() else {
        return Err("a key is not a struct".to_owned());
    };
    let field = s
        .field_mut(name)
        .ok_or_else(|| format!("a key has no field `{name}`"))?;
    field.try_apply(value).map_err(|e| format!("`{name}`: {e}"))
}

/// Replaces the items of `list` with `keys` (sorted by time). New items start from the key
/// type's `#[reflect(Default)]`, else from a copy of the first key, so fields the drawer does not
/// know keep a sensible value.
///
/// # Errors
///
/// When the keys are not this shape's kind, a time or value is not finite, or a new key cannot be
/// made (no default and no key to copy).
pub fn write_keys(
    registry: &TypeRegistry,
    list: &mut dyn List,
    shape: &KeyShape,
    keys: &Keys,
) -> Result<(), String> {
    let template = default_value(registry, shape.item)
        .map(|v| v.into_partial_reflect())
        .or_else(|| list.get(0).and_then(clone_value))
        .ok_or_else(|| "the key type has no #[reflect(Default)] and the list is empty".to_owned())?;
    let mut items = Vec::new();
    match (shape.kind, keys) {
        (ShapeKind::Curve { value, tangents }, Keys::Curve(points)) => {
            let mut points = points.clone();
            points.sort_by(|a, b| a.time.total_cmp(&b.time));
            for point in points {
                let finite = point.time.is_finite()
                    && point.value.is_finite()
                    && point.tangents.is_none_or(|(a, b)| a.is_finite() && b.is_finite());
                if !finite {
                    return Err(format!("a key needs finite numbers, got {point:?}"));
                }
                let mut item = clone_value(template.as_ref()).ok_or("the key type cannot be copied")?;
                set_field(item.as_mut(), shape.time, &point.time)?;
                set_field(item.as_mut(), value, &point.value)?;
                if tangents {
                    let (left, right) = point.tangents.unwrap_or((0.0, 0.0));
                    set_field(item.as_mut(), "in_tangent", &left)?;
                    set_field(item.as_mut(), "out_tangent", &right)?;
                }
                items.push(item);
            }
        }
        (ShapeKind::Gradient(storage), Keys::Gradient(stops)) => {
            let mut stops = stops.clone();
            stops.sort_by(|a, b| a.time.total_cmp(&b.time));
            for stop in stops {
                if !stop.time.is_finite() || !stop.color.iter().all(|c| c.is_finite()) {
                    return Err(format!("a stop needs finite numbers, got {stop:?}"));
                }
                let mut item = clone_value(template.as_ref()).ok_or("the key type cannot be copied")?;
                set_field(item.as_mut(), shape.time, &stop.time)?;
                let c = stop.color;
                match storage {
                    ColorStorage::Rgba => set_field(item.as_mut(), "color", &c)?,
                    ColorStorage::Rgb => set_field(item.as_mut(), "color", &[c[0], c[1], c[2]])?,
                    ColorStorage::Hex => set_field(item.as_mut(), "color", &super::color::to_hex(c, true))?,
                }
                items.push(item);
            }
        }
        _ => return Err("the keys are not this field's kind (curve or gradient)".to_owned()),
    }
    while list.pop().is_some() {}
    for item in items {
        list.push(item);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The adapter onto the animation curve canvas

/// The keys as the animation curve canvas's curve: keys with tangents are cubic, others linear.
#[must_use]
pub fn to_canvas(points: &[CurvePoint]) -> Curve {
    Curve::from(
        points
            .iter()
            .map(|p| {
                let kind = match p.tangents {
                    Some((left_tangent, right_tangent)) => CurveKeyKind::Cubic {
                        left_tangent,
                        right_tangent,
                    },
                    None => CurveKeyKind::Linear,
                };
                CurveKey::new(p.time, p.value, kind)
            })
            .collect::<Vec<_>>(),
    )
}

/// The canvas's curve back as keys (`tangents` when the key type has them; a key the canvas made
/// gets flat tangents).
#[must_use]
pub fn from_canvas(curve: &Curve, tangents: bool) -> Vec<CurvePoint> {
    curve
        .keys
        .iter()
        .map(|key| CurvePoint {
            time: key.location,
            value: key.value,
            tangents: tangents.then_some(match key.kind {
                CurveKeyKind::Cubic {
                    left_tangent,
                    right_tangent,
                } => (left_tangent, right_tangent),
                _ => (0.0, 0.0),
            }),
        })
        .collect()
}

/// The value of a gradient at `time` (linear interpolation between stops; the end stops hold).
#[must_use]
pub fn gradient_at(stops: &[GradientStop], time: f32) -> [f32; 4] {
    let Some(first) = stops.first() else {
        return [1.0; 4];
    };
    if time <= first.time {
        return first.color;
    }
    for pair in stops.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if time <= b.time {
            let span = (b.time - a.time).max(1e-6);
            let f = ((time - a.time) / span).clamp(0.0, 1.0);
            return std::array::from_fn(|i| a.color[i] + (b.color[i] - a.color[i]) * f);
        }
    }
    stops.last().map_or([1.0; 4], |s| s.color)
}

// ---------------------------------------------------------------------------------------------
// Presets

/// The curve presets: name and keys.
#[must_use]
pub fn curve_presets(tangents: bool) -> Vec<(&'static str, Vec<CurvePoint>)> {
    let key = |time, value, slope: f32| CurvePoint {
        time,
        value,
        tangents: tangents.then_some((slope, slope)),
    };
    vec![
        ("Constant", vec![key(0.0, 1.0, 0.0), key(1.0, 1.0, 0.0)]),
        ("Linear up", vec![key(0.0, 0.0, 1.0), key(1.0, 1.0, 1.0)]),
        ("Linear down", vec![key(0.0, 1.0, -1.0), key(1.0, 0.0, -1.0)]),
        ("Ease in-out", vec![key(0.0, 0.0, 0.0), key(1.0, 1.0, 0.0)]),
        (
            "Bell",
            vec![key(0.0, 0.0, 0.0), key(0.5, 1.0, 0.0), key(1.0, 0.0, 0.0)],
        ),
    ]
}

/// The gradient presets: name and stops.
#[must_use]
pub fn gradient_presets() -> Vec<(&'static str, Vec<GradientStop>)> {
    let stop = |time, color| GradientStop { time, color };
    vec![
        (
            "White to black",
            vec![stop(0.0, [1.0, 1.0, 1.0, 1.0]), stop(1.0, [0.0, 0.0, 0.0, 1.0])],
        ),
        (
            "Black to white",
            vec![stop(0.0, [0.0, 0.0, 0.0, 1.0]), stop(1.0, [1.0, 1.0, 1.0, 1.0])],
        ),
        (
            "Fade out",
            vec![stop(0.0, [1.0, 1.0, 1.0, 1.0]), stop(1.0, [1.0, 1.0, 1.0, 0.0])],
        ),
    ]
}

/// The preset called `name` for `shape` (case-insensitive).
#[must_use]
pub fn preset(shape: &KeyShape, name: &str) -> Option<Keys> {
    match shape.kind {
        ShapeKind::Curve { tangents, .. } => curve_presets(tangents)
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, keys)| Keys::Curve(keys)),
        ShapeKind::Gradient(_) => gradient_presets()
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, stops)| Keys::Gradient(stops)),
    }
}

/// The preset names of `shape`.
#[must_use]
pub fn preset_names(shape: &KeyShape) -> Vec<&'static str> {
    match shape.kind {
        ShapeKind::Curve { tangents, .. } => curve_presets(tangents).into_iter().map(|(n, _)| n).collect(),
        ShapeKind::Gradient(_) => gradient_presets().into_iter().map(|(n, _)| n).collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// The field

/// The canvas view and selected key of an open curve editor, kept in egui's memory.
#[derive(Clone, Copy, Default)]
struct CanvasState {
    view: crate::anim::view2d::View2d,
    selected: Option<usize>,
}

/// Paints the mini graph of `keys` into `rect`.
fn paint_mini(ui: &egui::Ui, rect: egui::Rect, keys: &Keys, focused: bool) {
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    painter.rect_filled(rect, 2.0, visuals.extreme_bg_color);
    match keys {
        Keys::Curve(points) => {
            let curve = to_canvas(points);
            let b = curve.bounds();
            let (t0, t1) = (b[0], b[2].max(b[0] + 1e-3));
            let (v0, v1) = (b[1].min(0.0), b[3].max(b[1] + 1e-3).max(1.0));
            let steps = (rect.width() / 3.0).max(8.0) as usize;
            let line: Vec<egui::Pos2> = (0..=steps)
                .map(|i| {
                    let t = t0 + (t1 - t0) * i as f32 / steps as f32;
                    let v = if points.is_empty() { 0.0 } else { curve.value_at(t) };
                    egui::pos2(
                        egui::lerp(rect.left()..=rect.right(), (t - t0) / (t1 - t0)),
                        egui::lerp(rect.bottom() - 2.0..=rect.top() + 2.0, (v - v0) / (v1 - v0)),
                    )
                })
                .collect();
            painter.add(egui::Shape::line(line, visuals.selection.stroke));
        }
        Keys::Gradient(stops) => {
            let columns = (rect.width() / 2.0).max(4.0) as usize;
            let w = rect.width() / columns as f32;
            for i in 0..columns {
                let t = (i as f32 + 0.5) / columns as f32;
                let c = gradient_at(stops, t);
                let byte = |x: f32| (super::color::encode(x) * 255.0).round() as u8;
                let color = egui::Color32::from_rgba_unmultiplied(
                    byte(c[0]),
                    byte(c[1]),
                    byte(c[2]),
                    (c[3].clamp(0.0, 1.0) * 255.0) as u8,
                );
                let x = rect.left() + w * i as f32;
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x, rect.top()),
                        egui::pos2(x + w + 0.5, rect.bottom()),
                    ),
                    0.0,
                    color,
                );
            }
        }
    }
    let stroke = if focused {
        visuals.selection.stroke
    } else {
        visuals.widgets.inactive.bg_stroke
    };
    painter.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
}

/// Draws a curve or gradient field; `None` when `value` is neither (the caller draws it).
pub fn curve_field_ui(
    value: &mut dyn PartialReflect,
    ui: &mut egui::Ui,
    id: egui::Id,
    registry: &TypeRegistry,
    context: &mut InspectorContext,
) -> Option<bool> {
    let shape = shape_of(registry, value)?;
    let ReflectMut::List(list) = value.reflect_mut() else {
        return None;
    };
    let keys = read_keys(list, &shape);
    let field = {
        let path = context.path();
        path.rsplit_once('.').map_or(path, |(_, last)| last).to_owned()
    };
    let name = if field.is_empty() {
        "Curve".to_owned()
    } else {
        field
    };
    let open_id = id.with(("curve_field_open", context.path().to_owned()));
    let mut open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
    let kind_word = match shape.kind {
        ShapeKind::Curve { .. } => "curve",
        ShapeKind::Gradient(_) => "gradient",
    };
    let mut edited: Option<(Keys, bool)> = None;
    ui.vertical(|ui| {
        let height = ui.spacing().interact_size.y * 1.4;
        let width = ui.available_width().clamp(120.0, 260.0);
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
        let response =
            crate::a11y_audit::name(response, egui::WidgetType::Button, &format!("{name} {kind_word}"))
                .on_hover_text(format!("{} keys; click (or Enter) to edit", list.len()));
        paint_mini(ui, rect, &keys, response.has_focus());
        if response.clicked() {
            open = !open;
        }
        if open {
            edited = editor_body(
                ui,
                id.with(("curve_field", context.path().to_owned())),
                &shape,
                &keys,
                &name,
                context,
            );
        }
    });
    ui.data_mut(|d| d.insert_temp(open_id, open));
    let Some((new_keys, structural)) = edited else {
        return Some(false);
    };
    if new_keys == keys {
        return Some(false);
    }
    match write_keys(registry, list, &shape, &new_keys) {
        Ok(()) => {
            context.record(structural);
            Some(true)
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "curve field: {err}");
            Some(false)
        }
    }
}

/// The open editor: the canvas (curves), the key table, **Add key** and presets. Returns the new
/// keys and whether the edit was structural (a key added or removed, a preset) when anything
/// changed.
fn editor_body(
    ui: &mut egui::Ui,
    id: egui::Id,
    shape: &KeyShape,
    keys: &Keys,
    name: &str,
    context: &mut InspectorContext,
) -> Option<(Keys, bool)> {
    let mut out = keys.clone();
    let mut structural = false;
    let mut changed = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        if let (Keys::Curve(points), ShapeKind::Curve { tangents, .. }) = (&mut out, shape.kind) {
            let state_id = id.with("canvas");
            let mut state = ui
                .data(|d| d.get_temp::<CanvasState>(state_id))
                .unwrap_or_default();
            let canvas = to_canvas(points);
            let size = egui::vec2(
                ui.available_width().clamp(160.0, 320.0),
                120.0 * ui.ctx().zoom_factor().max(1.0),
            );
            let action = ui
                .allocate_ui(size, |ui| {
                    crate::anim::curve_view::curve_editor(
                        ui,
                        state_id,
                        &canvas,
                        &mut state.view,
                        &mut state.selected,
                        None,
                    )
                })
                .inner;
            ui.data_mut(|d| d.insert_temp(state_id, state));
            if let Some(action) = action {
                let mut moved = canvas.clone();
                if crate::anim::curve_view::apply_action(&mut moved, &action).is_ok() && moved != canvas {
                    structural |= !matches!(action, crate::anim::curve_view::CurveAction::Move(..));
                    *points = from_canvas(&moved, tangents);
                    changed = true;
                }
            }
        }
        // The key table: every key is a row of focusable fields.
        let mut remove = None;
        match &mut out {
            Keys::Curve(points) => {
                for (index, point) in points.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        let row = format!("{name} key {}", index + 1);
                        changed |= number(ui, &mut point.time, "t", &row);
                        changed |= number(ui, &mut point.value, "v", &row);
                        if let Some((left, right)) = &mut point.tangents {
                            changed |= number(ui, left, "in", &row);
                            changed |= number(ui, right, "out", &row);
                        }
                        let button = ui.small_button("Remove");
                        if crate::a11y_audit::name(button, egui::WidgetType::Button, &format!("Remove {row}"))
                            .clicked()
                        {
                            remove = Some(index);
                        }
                    });
                }
            }
            Keys::Gradient(stops) => {
                for (index, stop) in stops.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        let row = format!("{name} stop {}", index + 1);
                        changed |= number(ui, &mut stop.time, "t", &row);
                        let picker_id = id.with(("stop", index));
                        super::color::picker_toggle(ui, picker_id, stop.color, &row);
                        let button = ui.small_button("Remove");
                        if crate::a11y_audit::name(button, egui::WidgetType::Button, &format!("Remove {row}"))
                            .clicked()
                        {
                            remove = Some(index);
                        }
                    });
                    let outcome = super::color::picker_panel(
                        ui,
                        id.with(("stop", index)),
                        &mut stop.color,
                        true,
                        false,
                        &format!("{name} stop {}", index + 1),
                        Some(&mut context.pickers),
                    );
                    changed |= outcome.changed;
                }
            }
        }
        if let Some(index) = remove {
            match &mut out {
                Keys::Curve(points) => {
                    points.remove(index);
                }
                Keys::Gradient(stops) => {
                    stops.remove(index);
                }
            }
            changed = true;
            structural = true;
        }
        ui.horizontal_wrapped(|ui| {
            let add = ui.button("Add key");
            if crate::a11y_audit::name(add, egui::WidgetType::Button, &format!("Add {name} key")).clicked() {
                add_key(&mut out);
                changed = true;
                structural = true;
            }
            ui.weak("Presets:");
            for preset_name in preset_names(shape) {
                let button = ui.small_button(preset_name);
                if crate::a11y_audit::name(
                    button,
                    egui::WidgetType::Button,
                    &format!("{name} preset {preset_name}"),
                )
                .clicked()
                    && let Some(keys) = preset(shape, preset_name)
                {
                    out = keys;
                    changed = true;
                    structural = true;
                }
            }
        });
    });
    changed.then_some((out, structural))
}

/// A key field: a drag value (arrow keys step it; typing sets it) with an accessible name.
fn number(ui: &mut egui::Ui, value: &mut f32, label: &str, row: &str) -> bool {
    let before = *value;
    let response = ui.add(
        egui::DragValue::new(value)
            .speed(0.01)
            .prefix(format!("{label} ")),
    );
    crate::a11y_audit::name(response, egui::WidgetType::DragValue, &format!("{row} {label}"));
    if !value.is_finite() {
        *value = before;
    }
    *value != before
}

/// Adds a key after the last one (a quarter of the span later, or at 0 / 1 for the first two),
/// with the curve's value there or the last stop's color.
pub fn add_key(keys: &mut Keys) {
    fn next_time(times: &[f32]) -> f32 {
        match times {
            [] => 0.0,
            [only] => only + 1.0,
            [first, .., last] => last + ((last - first) * 0.25).max(0.1),
        }
    }
    match keys {
        Keys::Curve(points) => {
            let times: Vec<f32> = points.iter().map(|p| p.time).collect();
            let time = next_time(&times);
            let value = if points.is_empty() {
                0.0
            } else {
                to_canvas(points).value_at(time)
            };
            let tangents = points.first().and_then(|p| p.tangents).map(|_| (0.0, 0.0));
            points.push(CurvePoint {
                time,
                value,
                tangents,
            });
        }
        Keys::Gradient(stops) => {
            let times: Vec<f32> = stops.iter().map(|s| s.time).collect();
            let time = next_time(&times);
            let color = stops.last().map_or([1.0; 4], |s| s.color);
            stops.push(GradientStop { time, color });
        }
    }
}
