// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from bevy-inspector-egui 0.37.0
// (https://github.com/jakobhellermann/bevy-inspector-egui, commit ac67298,
// crates/bevy-inspector-egui/src/inspector_egui_impls/{mod,std_impls,glam_impls}.rs),
// Copyright (c) Jakob Hellermann and bevy-inspector-egui contributors,
// licensed MIT OR Apache-2.0; used here under the MIT licence:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Widgets for concrete leaf types: numbers, booleans, strings (with the asset picker), paths,
//! durations, ranges, glam vectors/matrices/quaternions, and colours.
//!
//! Upstream stores the widget table as type data in the app's type registry. Regia Engine keeps a
//! process-wide [`InspectorImpls`] table instead (the editor's registry is shared with the
//! running game and the agent protocol), and still honours an [`InspectorEguiImpl`] a game
//! registers as type data, so custom drawers need no editor change.

use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use glam::{
    BVec2, BVec3, BVec4, DMat2, DMat3, DMat4, DVec2, DVec3, DVec4, EulerRot, IVec2, IVec3, IVec4, Mat2, Mat3,
    Mat3A, Mat4, Quat, UVec2, UVec3, UVec4, Vec2, Vec3, Vec3A, Vec4,
};
use regia_ecs::reflect::{CreateTypeData, PartialReflect, Reflect};

use super::options::{FieldOptions, NumberDisplay, NumberOptions, QuatDisplay, QuatOptions};
use super::picker_links::PickerRequest;
use super::reflect_ui::{InspectorContext, InspectorUi};
use crate::dnd::ContentKind;

/// A widget function: edits `value` (downcast to the concrete type) and returns whether it
/// changed.
pub type InspectorEguiImplFn = fn(&mut dyn Any, &mut egui::Ui, &dyn Any, egui::Id, InspectorUi<'_>) -> bool;

/// A custom widget for a concrete reflected type. Implement it and register
/// `InspectorEguiImpl` as the type's data to replace the generic struct/enum drawer.
pub trait InspectorPrimitive: Reflect {
    /// Draws the widget; returns whether the value changed.
    fn ui(&mut self, ui: &mut egui::Ui, options: &dyn Any, id: egui::Id, env: InspectorUi<'_>) -> bool;
}

fn ui_vtable<T: InspectorPrimitive>(
    value: &mut dyn Any,
    ui: &mut egui::Ui,
    options: &dyn Any,
    id: egui::Id,
    env: InspectorUi<'_>,
) -> bool {
    match value.downcast_mut::<T>() {
        Some(value) => T::ui(value, ui, options, id, env),
        None => false,
    }
}

/// Function pointer for drawing one concrete type, stored per type.
#[derive(Clone, Copy)]
pub struct InspectorEguiImpl {
    fn_mut: InspectorEguiImplFn,
}

impl std::fmt::Debug for InspectorEguiImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InspectorEguiImpl")
    }
}

impl<T: InspectorPrimitive> CreateTypeData<T> for InspectorEguiImpl {
    fn create_type_data(_: ()) -> Self {
        Self {
            fn_mut: ui_vtable::<T>,
        }
    }
}

impl InspectorEguiImpl {
    /// A widget from a raw function.
    #[must_use]
    pub fn new(fn_mut: InspectorEguiImplFn) -> Self {
        Self { fn_mut }
    }

    /// Runs the widget.
    pub fn execute(
        &self,
        value: &mut dyn Any,
        ui: &mut egui::Ui,
        options: &dyn Any,
        id: egui::Id,
        env: InspectorUi<'_>,
    ) -> bool {
        (self.fn_mut)(value, ui, options, id, env)
    }
}

/// The built-in widget table: std and glam leaf types.
pub struct InspectorImpls {
    by_type: HashMap<TypeId, InspectorEguiImpl>,
}

impl Default for InspectorImpls {
    fn default() -> Self {
        let mut impls = Self {
            by_type: HashMap::new(),
        };
        impls.register_std();
        impls.register_glam();
        impls
    }
}

impl InspectorImpls {
    /// The process-wide table.
    pub fn global() -> &'static Self {
        static IMPLS: OnceLock<InspectorImpls> = OnceLock::new();
        IMPLS.get_or_init(Self::default)
    }

    /// The widget for a type.
    #[must_use]
    pub fn get(&self, type_id: TypeId) -> Option<&InspectorEguiImpl> {
        self.by_type.get(&type_id)
    }

    /// Adds (or replaces) the widget of `T`.
    pub fn insert<T: 'static>(&mut self, fn_mut: InspectorEguiImplFn) {
        self.by_type
            .insert(TypeId::of::<T>(), InspectorEguiImpl::new(fn_mut));
    }

    fn add<T: InspectorPrimitive>(&mut self) {
        self.by_type.insert(
            TypeId::of::<T>(),
            <InspectorEguiImpl as CreateTypeData<T>>::create_type_data(()),
        );
    }

    fn register_std(&mut self) {
        self.insert::<f32>(number_ui::<f32>);
        self.insert::<f64>(number_ui::<f64>);
        self.insert::<i8>(number_ui::<i8>);
        self.insert::<i16>(number_ui::<i16>);
        self.insert::<i32>(number_ui::<i32>);
        self.insert::<i64>(number_ui::<i64>);
        self.insert::<isize>(number_ui::<isize>);
        self.insert::<u8>(number_ui::<u8>);
        self.insert::<u16>(number_ui::<u16>);
        self.insert::<u32>(number_ui::<u32>);
        self.insert::<u64>(number_ui::<u64>);
        self.insert::<usize>(number_ui::<usize>);
        self.add::<bool>();
        self.add::<String>();
        self.add::<Cow<'static, str>>();
        self.add::<PathBuf>();
        self.add::<Duration>();
        self.add::<regia_ecs::prelude::Entity>();
        self.add::<std::ops::Range<f32>>();
        self.add::<std::ops::Range<f64>>();
        self.add::<std::ops::RangeInclusive<f32>>();
        self.add::<std::ops::RangeInclusive<f64>>();
        self.insert::<regia_scene::components::Layers>(scene_layers_ui);
    }

    fn register_glam(&mut self) {
        self.insert::<Vec2>(vec2_ui);
        self.insert::<Vec3>(vec3_ui);
        self.insert::<Vec3A>(vec3a_ui);
        self.insert::<Vec4>(vec4_ui);
        self.insert::<UVec2>(uvec2_ui);
        self.insert::<UVec3>(uvec3_ui);
        self.insert::<UVec4>(uvec4_ui);
        self.insert::<IVec2>(ivec2_ui);
        self.insert::<IVec3>(ivec3_ui);
        self.insert::<IVec4>(ivec4_ui);
        self.insert::<DVec2>(dvec2_ui);
        self.insert::<DVec3>(dvec3_ui);
        self.insert::<DVec4>(dvec4_ui);
        self.insert::<BVec2>(bvec2_ui);
        self.insert::<BVec3>(bvec3_ui);
        self.insert::<BVec4>(bvec4_ui);
        self.insert::<Mat2>(mat2_ui);
        self.insert::<Mat3>(mat3_ui);
        self.insert::<Mat3A>(mat3a_ui);
        self.insert::<Mat4>(mat4_ui);
        self.insert::<DMat2>(dmat2_ui);
        self.insert::<DMat3>(dmat3_ui);
        self.insert::<DMat4>(dmat4_ui);
        self.insert::<Quat>(quat_ui);
    }
}

// ---------------------------------------------------------------------------------------------
// Numbers

/// The number options for `T` out of a field's options, and whether the value is radians edited
/// as degrees.
fn resolve_number_options<T: egui::emath::Numeric>(options: &dyn Any) -> (NumberOptions<T>, bool) {
    if let Some(options) = options.downcast_ref::<NumberOptions<T>>() {
        return (options.clone(), false);
    }
    if let Some(field) = options.downcast_ref::<FieldOptions>() {
        return (
            field.number_options::<T>(),
            field.radians_as_degrees() && !T::INTEGRAL,
        );
    }
    (NumberOptions::default(), false)
}

/// How long a refused expression keeps its field outlined in the error color (seconds).
pub const EXPR_FLASH_SECONDS: f64 = 0.8;

/// The egui memory key of the last refused expression's message ([`last_expr_refusal`]).
const LAST_EXPR_REFUSAL: &str = "regia_inspector_last_expr_refusal";

/// The message of the last expression a number field refused in `ctx` (the field shows it on
/// hover while it flashes).
#[must_use]
pub fn last_expr_refusal(ctx: &egui::Context) -> Option<String> {
    ctx.data(|data| data.get_temp::<String>(egui::Id::new(LAST_EXPR_REFUSAL)))
}

/// The text a number field last committed and the frame it did (see `display_number`).
#[derive(Clone, Debug)]
struct ExprCommit {
    text: String,
    frame: u64,
}

/// A refused expression on one number field: when it was typed and why it was refused.
#[derive(Clone, Debug)]
struct ExprFlash {
    at: f64,
    message: String,
}

/// The step Ctrl (Cmd on macOS) snaps a dragged number to: the field's `@Step` (it sets the drag
/// speed), else 1 for whole numbers and 0.1 otherwise.
#[must_use]
pub fn snap_step(speed: f32, integral: bool) -> f64 {
    if speed > 0.0 && speed.is_finite() {
        f64::from(speed)
    } else if integral {
        1.0
    } else {
        0.1
    }
}

/// `value` snapped to the nearest multiple of `step` (unchanged for a step that is not positive).
#[must_use]
pub fn snap_to_step(value: f64, step: f64) -> f64 {
    if step > 0.0 && step.is_finite() {
        let snapped = (value / step).round() * step;
        if snapped.is_finite() { snapped } else { value }
    } else {
        value
    }
}

/// The drag speed under the held modifiers: Alt drags ten times faster (Shift's slow drag is the
/// widget's own: a tenth).
#[must_use]
pub fn modified_speed(speed: f64, modifiers: egui::Modifiers) -> f64 {
    if modifiers.alt { speed * 10.0 } else { speed }
}

/// What a number field's text box makes of `text` for a field holding `current` with display
/// suffix `suffix`: a plain number, or an expression ([`super::expr`]). Whole-number fields round.
///
/// # Errors
///
/// The expression's [`super::expr::ExprError`].
pub fn parse_number_text(
    text: &str,
    current: f64,
    suffix: &str,
    integral: bool,
) -> Result<f64, super::expr::ExprError> {
    let trimmed: String = text.trim().replace('\u{2212}', "-");
    let value = if super::expr::is_plain_number(&trimmed) {
        trimmed.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        super::expr::evaluate(&trimmed, current, suffix)?
    };
    let value = if integral { value.round() } else { value };
    if value.is_finite() {
        Ok(value)
    } else {
        Err(super::expr::ExprError {
            code: "expr_not_finite",
            message: "the result is not a finite number".into(),
        })
    }
}

/// Edits a number with options: drag or slider, clamped to `min`/`max`.
///
/// The typed text may be an expression with units ([`super::expr`]); a refused one keeps the old
/// value and outlines the field in the error color for [`EXPR_FLASH_SECONDS`] with the reason on
/// hover. While dragging, Shift is slow, Alt fast ([`modified_speed`]) and Ctrl (Cmd) snaps to
/// the step ([`snap_step`]).
pub fn display_number<T: egui::emath::Numeric>(
    value: &mut T,
    options: &NumberOptions<T>,
    ui: &mut egui::Ui,
) -> bool {
    let default_speed = if T::INTEGRAL { 0.25 } else { 0.05 };
    let before = *value;
    let mut changed = match options.display {
        NumberDisplay::Drag => {
            let current = value.to_f64();
            let refused: std::cell::RefCell<Option<super::expr::ExprError>> = std::cell::RefCell::new(None);
            let suffix = options.suffix.as_str();
            let modifiers = ui.input(|i| i.modifiers);
            let base_speed = if options.speed != 0.0 {
                f64::from(options.speed)
            } else {
                default_speed
            };
            // The drag value's id: it parses a committed text twice (in the frame the edit ends and
            // again in the next frame's lost-focus pass), so the commit is remembered and the
            // repeat keeps the value instead of applying a relative edit (`*=4`) a second time.
            let commit_id = ui.next_auto_id().with("expr_commit");
            let frame = ui.ctx().cumulative_frame_nr();
            let last_commit: Option<ExprCommit> = ui.data(|data| data.get_temp(commit_id));
            let committed: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
            let mut widget = egui::DragValue::new(value)
                // Parse on commit only: a relative edit (`+10`) must apply once, not per keystroke.
                .update_while_editing(false)
                .custom_parser(|text| {
                    if let Some(last) = &last_commit
                        && last.text == text
                        && frame <= last.frame + 1
                    {
                        return Some(current);
                    }
                    *committed.borrow_mut() = Some(text.to_owned());
                    match parse_number_text(text, current, suffix, T::INTEGRAL) {
                        Ok(parsed) => Some(parsed),
                        Err(error) => {
                            *refused.borrow_mut() = Some(error);
                            None
                        }
                    }
                });
            if !options.prefix.is_empty() {
                widget = widget.prefix(&options.prefix);
            }
            if !options.suffix.is_empty() {
                widget = widget.suffix(&options.suffix);
            }
            match (options.min, options.max) {
                (Some(min), Some(max)) => widget = widget.range(min.to_f64()..=max.to_f64()),
                (Some(min), None) => widget = widget.range(min.to_f64()..=f64::MAX),
                (None, Some(max)) => widget = widget.range(f64::MIN..=max.to_f64()),
                (None, None) => {}
            }
            widget = widget.speed(modified_speed(base_speed, modifiers));
            let mut response = ui.add(widget);
            if let Some(text) = committed.into_inner() {
                ui.data_mut(|data| data.insert_temp(commit_id, ExprCommit { text, frame }));
            }
            let flash_id = response.id.with("expr_flash");
            let now = ui.input(|i| i.time);
            if let Some(error) = refused.into_inner() {
                ui.data_mut(|data| {
                    data.insert_temp(egui::Id::new(LAST_EXPR_REFUSAL), error.message.clone());
                    data.insert_temp(
                        flash_id,
                        ExprFlash {
                            at: now,
                            message: error.message,
                        },
                    );
                });
            }
            if let Some(flash) = ui.data(|data| data.get_temp::<ExprFlash>(flash_id)) {
                if now - flash.at <= EXPR_FLASH_SECONDS {
                    let error = crate::theme::ui_tokens(ui.ctx()).error;
                    ui.painter().rect_stroke(
                        response.rect,
                        2.0,
                        egui::Stroke::new(2.0, error),
                        egui::StrokeKind::Outside,
                    );
                    response = response.on_hover_text(format!("Not applied: {}", flash.message));
                    ui.ctx().request_repaint();
                } else {
                    ui.data_mut(|data| data.remove::<ExprFlash>(flash_id));
                }
            }
            // While text-editing, the response reports every keystroke as a change; the value only
            // moves when the edit is committed.
            let mut drag_changed = value.to_f64() != current;
            // Ctrl (Cmd) while dragging snaps to the field's step.
            if drag_changed && response.dragged() && modifiers.command {
                let step = snap_step(options.speed, T::INTEGRAL);
                *value = T::from_f64(snap_to_step(value.to_f64(), step));
                drag_changed = value.to_f64() != current;
            }
            drag_changed
        }
        NumberDisplay::Slider => {
            let min = options.min.unwrap_or_else(|| T::from_f64(0.0));
            let max = options.max.unwrap_or_else(|| T::from_f64(1.0));
            ui.add(egui::Slider::new(value, min..=max)).changed()
        }
    };
    if let Some(min) = options.min
        && value.to_f64() < min.to_f64()
    {
        *value = min;
        changed = true;
    }
    if let Some(max) = options.max
        && value.to_f64() > max.to_f64()
    {
        *value = max;
        changed = true;
    }
    // Typed text such as `inf`, `nan` or `1e40` (past f32) would put a non-finite number into
    // the field (found by the monkey test, ADR-3290): keep the old value instead.
    if !value.to_f64().is_finite() {
        *value = before;
        changed = false;
    }
    changed
}

/// Edits a number, converting radians to degrees for display when the options say so.
fn number_with_options<T: egui::emath::Numeric>(
    value: &mut T,
    options: &NumberOptions<T>,
    degrees: bool,
    ui: &mut egui::Ui,
) -> bool {
    if !degrees {
        return display_number(value, options, ui);
    }
    let mut shown = value.to_f64().to_degrees();
    let shown_options = options.map(|v| v.to_f64());
    let changed = display_number(&mut shown, &shown_options, ui);
    let radians = T::from_f64(shown.to_radians());
    // A finite f64 can still overflow the field's type.
    if !changed || !radians.to_f64().is_finite() {
        return false;
    }
    *value = radians;
    true
}

/// Draws the mixed marker when this path differs across the selection.
fn mixed_marker(ui: &mut egui::Ui, mixed: bool) {
    if mixed {
        ui.label("—");
    }
}

/// The value a label drag of `points` (right and up positive) makes of `current`: the field's
/// drag speed per point, Shift a tenth, Alt ten times, Ctrl (Cmd) snapped to the step, then
/// clamped to `min..=max` and rounded for whole numbers.
#[must_use]
pub fn scrub_value(
    current: f64,
    points: f64,
    speed: f32,
    modifiers: egui::Modifiers,
    integral: bool,
    range: (Option<f64>, Option<f64>),
) -> f64 {
    let base = if speed != 0.0 {
        f64::from(speed)
    } else if integral {
        0.25
    } else {
        0.05
    };
    let mut per_point = modified_speed(base, modifiers);
    if modifiers.shift_only() {
        per_point /= 10.0;
    }
    let mut next = current + points * per_point;
    if modifiers.command {
        next = snap_to_step(next, snap_step(speed, integral));
    }
    if let Some(min) = range.0 {
        next = next.max(min);
    }
    if let Some(max) = range.1 {
        next = next.min(max);
    }
    if integral {
        next = next.round();
    }
    if next.is_finite() { next } else { current }
}

/// Applies a label drag to a number (in the displayed unit: degrees for a radian angle).
fn scrub_number<T: egui::emath::Numeric>(
    value: &mut T,
    options: &NumberOptions<T>,
    degrees: bool,
    (points, modifiers): (f64, egui::Modifiers),
) -> bool {
    let shown = if degrees {
        value.to_f64().to_degrees()
    } else {
        value.to_f64()
    };
    let next = scrub_value(
        shown,
        points,
        options.speed,
        modifiers,
        T::INTEGRAL,
        (options.min.map(|v| v.to_f64()), options.max.map(|v| v.to_f64())),
    );
    if next == shown {
        return false;
    }
    let stored = T::from_f64(if degrees { next.to_radians() } else { next });
    if !stored.to_f64().is_finite() || stored.to_f64() == value.to_f64() {
        return false;
    }
    *value = stored;
    true
}

/// An integer field drawn as flags (`@Flags`) or as the bit-layer grid (`@LayerMask`); `None`
/// when the field is neither, or not an integer.
fn bit_field_ui(
    value: &mut dyn Any,
    ui: &mut egui::Ui,
    field: &FieldOptions,
    id: egui::Id,
    env: &mut InspectorUi<'_>,
) -> Option<bool> {
    let names = field.attrs.flag_names();
    let domain = field.attrs.layer_mask;
    if names.is_empty() && domain.is_none() {
        return None;
    }
    let (bits, width) = super::flags::read_bits(value)?;
    mixed_marker(ui, env.context.is_mixed());
    let spoken = env.field_name();
    let next = match domain {
        Some(domain) => {
            let layers = super::flags::layer_names(domain, env.context.project_root.as_deref(), width);
            super::flags::layer_grid(ui, id, bits, &layers[..layers.len().min(width as usize)], &spoken)
        }
        None => super::flags::flags_ui(ui, bits, &names, &spoken),
    };
    Some(next.is_some_and(|next| super::flags::write_bits(value, next)))
}

/// The scene's `Layers` component: the bit-layer grid with the project's layer names.
fn scene_layers_ui(
    value: &mut dyn Any,
    ui: &mut egui::Ui,
    _: &dyn Any,
    id: egui::Id,
    env: InspectorUi<'_>,
) -> bool {
    let Some(layers) = value.downcast_mut::<regia_scene::components::Layers>() else {
        return false;
    };
    mixed_marker(ui, env.context.is_mixed());
    let names = super::flags::layer_names("physics", env.context.project_root.as_deref(), u32::BITS);
    let spoken = env.field_name();
    match super::flags::layer_grid(
        ui,
        id,
        u64::from(layers.0),
        &names[..names.len().min(32)],
        &spoken,
    ) {
        Some(next) => {
            layers.0 = next as u32;
            true
        }
        None => false,
    }
}

/// The widget for every primitive number type: flags and layer masks for integers with those
/// attributes, a drag value otherwise (which also takes a drag over the field's label).
pub fn number_ui<T: egui::emath::Numeric>(
    value: &mut dyn Any,
    ui: &mut egui::Ui,
    options: &dyn Any,
    id: egui::Id,
    mut env: InspectorUi<'_>,
) -> bool {
    if T::INTEGRAL
        && let Some(field) = options.downcast_ref::<FieldOptions>()
        && let Some(changed) = bit_field_ui(value, ui, field, id, &mut env)
    {
        return changed;
    }
    let Some(value) = value.downcast_mut::<T>() else {
        return false;
    };
    mixed_marker(ui, env.context.is_mixed());
    let (options, degrees) = resolve_number_options::<T>(options);
    let scrubbed = env
        .context
        .take_label_scrub()
        .is_some_and(|scrub| scrub_number(value, &options, degrees, scrub));
    number_with_options(value, &options, degrees, ui) || scrubbed
}

// ---------------------------------------------------------------------------------------------
// Std leaf types

impl InspectorPrimitive for bool {
    fn ui(&mut self, ui: &mut egui::Ui, options: &dyn Any, _: egui::Id, env: InspectorUi<'_>) -> bool {
        mixed_marker(ui, env.context.is_mixed());
        // The checkbox's visible label is the separate field label beside it; the checkbox is
        // named after the field (its display name when the attributes give one) so screen
        // readers announce what it toggles (FU-6, `docs/evidence/editor-a11y-2026-11.md`).
        // Inside a collection the element's own path segment (`tags[2]`) is the name, not the
        // collection field's.
        let spoken = options
            .downcast_ref::<FieldOptions>()
            .and_then(|f| f.attrs.display_name)
            .filter(|_| !env.context.path().ends_with(']'))
            .map(str::to_string)
            .unwrap_or_else(|| env.field_name());
        crate::a11y_audit::bare_checkbox(ui, self, &spoken).changed()
    }
}

impl InspectorPrimitive for String {
    fn ui(&mut self, ui: &mut egui::Ui, options: &dyn Any, id: egui::Id, env: InspectorUi<'_>) -> bool {
        mixed_marker(ui, env.context.is_mixed());
        let field = options.downcast_ref::<FieldOptions>();
        if let Some(filter) = field.and_then(|f| f.attrs.asset_filter) {
            return asset_picker(self, ui, filter, id, env);
        }
        if let Some(field) = field
            && (field.attrs.file_picker.is_some() || field.attrs.dir_picker)
        {
            let pick = match field.attrs.file_picker {
                Some(filter) => PathPick::File(filter),
                None => PathPick::Dir,
            };
            return path_picker(self, ui, pick, id, env);
        }
        // Named after its field, like the checkbox (FU-6), so a screen reader says what the text
        // field edits: the label beside it is not tied to it (review #10 of ADR-7746, ADR-7795).
        let spoken = field
            .and_then(|f| f.attrs.display_name)
            .filter(|_| !env.context.path().ends_with(']'))
            .map(str::to_string)
            .unwrap_or_else(|| env.field_name());
        let response = if field.is_some_and(|f| f.attrs.multiline) || self.contains('\n') {
            ui.text_edit_multiline(self)
        } else {
            ui.text_edit_singleline(self)
        };
        crate::a11y_bridge::name_node(&response, &spoken);
        response.changed()
    }
}

/// What a `@FilePicker` / `@DirPicker` field picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathPick<'a> {
    /// A file matching the filter (globs separated by `;` or `,`; empty takes every file).
    File(&'a str),
    /// A folder.
    Dir,
}

/// The most entries a path picker lists (a huge project still opens the picker at once).
pub const PATH_PICKER_LIMIT: usize = 5000;

/// Whether project-relative `path` matches a `@FilePicker` filter: `*.ext` globs (case
/// insensitive) or a plain extension, separated by `;` or `,`; an empty filter takes every file.
#[must_use]
pub fn file_filter_matches(filter: &str, path: &str) -> bool {
    let lowered = path.to_ascii_lowercase();
    let mut parts = filter
        .split([';', ','])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .peekable();
    if parts.peek().is_none() {
        return true;
    }
    parts.any(|part| {
        let part = part.to_ascii_lowercase();
        match part.strip_prefix('*') {
            Some(suffix) => lowered.ends_with(suffix),
            None => lowered.ends_with(&format!(".{}", part.trim_start_matches('.'))),
        }
    })
}

/// The project-relative paths (with `/`) a path picker offers under `root`, sorted: files matching
/// the filter, or folders. Hidden entries (`.git`, `.regia`) and build output (`target`) are
/// skipped, symbolic links are not followed (the picker never leaves the project), and at most
/// [`PATH_PICKER_LIMIT`] are listed.
#[must_use]
pub fn project_paths(root: &std::path::Path, pick: PathPick<'_>) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if kind.is_dir() {
                if pick == PathPick::Dir {
                    out.push(rel);
                }
                stack.push(path);
            } else if let PathPick::File(filter) = pick
                && file_filter_matches(filter, &rel)
            {
                out.push(rel);
            }
            if out.len() >= PATH_PICKER_LIMIT {
                out.sort();
                return out;
            }
        }
    }
    out.sort();
    out
}

/// A string or path field with `@FilePicker` / `@DirPicker` (ADR-7785): the text box and a
/// browse button whose picker lists only the open project's files or folders, with a search.
fn path_picker(
    value: &mut String,
    ui: &mut egui::Ui,
    pick: PathPick<'_>,
    id: egui::Id,
    env: InspectorUi<'_>,
) -> bool {
    let field = env.field_name();
    let search_id = id.with("path_picker_search");
    let list_id = id.with("path_picker_list");
    let mut changed = false;
    ui.horizontal(|ui| {
        let hint = match pick {
            PathPick::File(filter) if !filter.is_empty() => filter,
            PathPick::File(_) => "file",
            PathPick::Dir => "folder",
        };
        let response = ui.add(
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .desired_width(180.0),
        );
        changed |= crate::a11y_audit::name(response, egui::WidgetType::TextEdit, &field).changed();
        let spoken = match pick {
            PathPick::File(_) => format!("Browse for {field} (project files)"),
            PathPick::Dir => format!("Browse for {field} (project folders)"),
        };
        let root = env.context.project_root.clone();
        let menu = ui.menu_button("\u{1f4c2}", |ui| {
            let Some(root) = root else {
                ui.weak("Open a project to browse its files.");
                return;
            };
            let mut search = ui.data(|d| d.get_temp::<String>(search_id)).unwrap_or_default();
            let field_box = ui.add(egui::TextEdit::singleline(&mut search).hint_text("search"));
            crate::a11y_audit::name(field_box, egui::WidgetType::TextEdit, &format!("Search {field}"));
            ui.data_mut(|d| d.insert_temp(search_id, search.clone()));
            let paths: std::sync::Arc<Vec<String>> = match ui.data(|d| d.get_temp(list_id)) {
                Some(paths) => paths,
                None => {
                    let paths = std::sync::Arc::new(project_paths(&root, pick));
                    ui.data_mut(|d| d.insert_temp(list_id, std::sync::Arc::clone(&paths)));
                    paths
                }
            };
            let needle = search.trim().to_ascii_lowercase();
            let shown: Vec<&String> = paths
                .iter()
                .filter(|p| needle.is_empty() || p.to_ascii_lowercase().contains(&needle))
                .collect();
            if shown.is_empty() {
                ui.weak("Nothing in the project matches.");
            }
            let mut picked = None;
            crate::virtual_list::rows(
                ui,
                id.with("path_picker_rows"),
                shown.len(),
                crate::virtual_list::DROP_DOWN_HEIGHT,
                |ui, i| {
                    if ui
                        .selectable_label(*shown[i] == *value, shown[i].as_str())
                        .clicked()
                    {
                        picked = Some(shown[i].clone());
                    }
                },
            );
            if let Some(path) = picked {
                if *value != path {
                    *value = path;
                    changed = true;
                }
                ui.close();
            }
        });
        if menu.inner.is_none() {
            // Closed: list the project afresh next time it opens.
            ui.data_mut(|d| d.remove::<std::sync::Arc<Vec<String>>>(list_id));
        }
        crate::a11y_audit::name(menu.response, egui::WidgetType::Button, &spoken);
    });
    changed
}

impl InspectorPrimitive for Cow<'static, str> {
    fn ui(&mut self, ui: &mut egui::Ui, options: &dyn Any, id: egui::Id, env: InspectorUi<'_>) -> bool {
        let mut owned = self.to_string();
        let changed = owned.ui(ui, options, id, env);
        if changed {
            *self = Cow::Owned(owned);
        }
        changed
    }
}

impl InspectorPrimitive for PathBuf {
    fn ui(&mut self, ui: &mut egui::Ui, options: &dyn Any, id: egui::Id, env: InspectorUi<'_>) -> bool {
        let mut text = self.to_string_lossy().into_owned();
        let changed = text.ui(ui, options, id, env);
        if changed {
            *self = PathBuf::from(text);
        }
        changed
    }
}

impl InspectorPrimitive for Duration {
    fn ui(&mut self, ui: &mut egui::Ui, _: &dyn Any, _: egui::Id, _: InspectorUi<'_>) -> bool {
        let mut seconds = self.as_secs_f64();
        let options = NumberOptions {
            min: Some(0.0f64),
            suffix: " s".to_string(),
            speed: 0.01,
            ..NumberOptions::default()
        };
        let changed = display_number(&mut seconds, &options, ui);
        if changed {
            *self = Duration::from_secs_f64(seconds.max(0.0));
        }
        changed
    }
}

/// Edits `start` and `end` of a range side by side.
fn display_range<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    options: &dyn Any,
    symbol: &str,
    start: &mut T,
    end: &mut T,
) -> bool {
    let (options, degrees) = resolve_number_options::<T>(options);
    let mut changed = false;
    ui.horizontal(|ui| {
        changed |= number_with_options(start, &options, degrees, ui);
        ui.label(symbol);
        changed |= number_with_options(end, &options, degrees, ui);
    });
    changed
}

macro_rules! range_ui {
    ($($ty:ty),*) => {$(
        impl InspectorPrimitive for std::ops::Range<$ty> {
            fn ui(&mut self, ui: &mut egui::Ui, options: &dyn Any, _: egui::Id, _: InspectorUi<'_>) -> bool {
                let std::ops::Range { start, end } = self;
                display_range(ui, options, "..", start, end)
            }
        }

        impl InspectorPrimitive for std::ops::RangeInclusive<$ty> {
            fn ui(&mut self, ui: &mut egui::Ui, options: &dyn Any, _: egui::Id, _: InspectorUi<'_>) -> bool {
                let mut start = *self.start();
                let mut end = *self.end();
                let changed = display_range(ui, options, "..=", &mut start, &mut end);
                if changed {
                    *self = start..=end;
                }
                changed
            }
        }
    )*};
}

range_ui!(f32, f64);

// ---------------------------------------------------------------------------------------------
// Asset picker

/// Whether a project asset matches an `@AssetFilter`: comma-separated globs (`"*.ogg"`,
/// `"*.material.ron"`) or kind names (`mesh`, `level`, `prefab`, `material`, `texture`, `audio`).
/// `material` takes standard materials, material graphs and graph instances (the three files a
/// `MeshSource.material` loads); `mesh` takes glTF/GLB files and the built-in primitives
/// (`builtin:cube`, ...).
#[must_use]
pub fn asset_matches(filter: &str, path: &str, kind: ContentKind) -> bool {
    let lowered = path.to_ascii_lowercase();
    filter.split([',', ';']).map(str::trim).any(|part| {
        if let Some(suffix) = part.strip_prefix('*') {
            return lowered.ends_with(&suffix.to_ascii_lowercase());
        }
        let extensions: &[&str] = match part.to_ascii_lowercase().as_str() {
            "mesh" | "model" => return kind == ContentKind::Mesh || BUILTIN_MESHES.contains(&path),
            "level" => return kind == ContentKind::Level,
            "prefab" => return kind == ContentKind::Prefab,
            "material" => &[".material.ron", ".matgraph.ron", ".matinst.ron"],
            "texture" | "image" => &[".png", ".jpg", ".jpeg", ".ktx2", ".exr", ".hdr", ".tga"],
            "audio" | "sound" => &[".ogg", ".wav", ".flac", ".mp3"],
            "" => return false,
            other => return lowered.ends_with(&format!(".{other}")),
        };
        extensions.iter().any(|ext| lowered.ends_with(ext))
    })
}

/// The built-in primitives a mesh field takes besides project meshes (`regia_render::MeshSource`'s
/// `BUILTIN_*`).
pub const BUILTIN_MESHES: [&str; 3] = ["builtin:cube", "builtin:plane", "builtin:sphere"];

/// Whether `filter` names the mesh kind (its picker also offers [`BUILTIN_MESHES`]).
fn filter_takes_meshes(filter: &str) -> bool {
    filter
        .split([',', ';'])
        .map(str::trim)
        .any(|part| part.eq_ignore_ascii_case("mesh") || part.eq_ignore_ascii_case("model"))
}

/// What an asset picker lists for `filter`: the built-in primitives first for a mesh field, then
/// the matching project assets in catalog order, narrowed to those whose path contains `search`
/// (case-insensitive; empty lists everything).
#[must_use]
pub fn asset_choices<'a>(filter: &str, search: &str, assets: &'a [(String, ContentKind)]) -> Vec<&'a str> {
    let search = search.trim().to_ascii_lowercase();
    let wanted = |path: &str| search.is_empty() || path.to_ascii_lowercase().contains(&search);
    let builtins: &[&str] = if filter_takes_meshes(filter) {
        &BUILTIN_MESHES
    } else {
        &[]
    };
    builtins
        .iter()
        .copied()
        .filter(|path| wanted(path))
        .chain(
            assets
                .iter()
                .filter(|(path, kind)| asset_matches(filter, path, *kind) && wanted(path))
                .map(|(path, _)| path.as_str()),
        )
        .collect()
}

/// Whether an asset dragged from the Content Browser may be dropped on a field with `filter`.
///
/// # Errors
///
/// The reason the inspector shows under the field: `<path> is not a <filter kind> (<filter>)`.
pub fn asset_drop_verdict(filter: &str, path: &str, kind: ContentKind) -> Result<(), String> {
    if asset_matches(filter, path, kind) {
        return Ok(());
    }
    let wanted = filter
        .split([',', ';'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| match part.strip_prefix('*') {
            Some(suffix) => suffix.to_string(),
            None => part.to_ascii_lowercase(),
        })
        .collect::<Vec<_>>()
        .join(" or ");
    let article = if wanted.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    Err(format!("{path} is not {article} {wanted} ({filter})"))
}

/// The accessible name of the asset field being drawn: `<Component>.<field path>`.
fn asset_field_name(context: &InspectorContext) -> String {
    let component = context
        .component
        .rsplit("::")
        .next()
        .unwrap_or(&context.component);
    if context.path().is_empty() {
        component.to_string()
    } else {
        format!("{component}.{}", context.path())
    }
}

/// Edge of a picker thumbnail, in points.
const PICKER_THUMB: f32 = 20.0;

/// A string field with `@AssetFilter` (ADR-7472): the current picture, a text box that takes a
/// drop from the Content Browser (refused with a visible reason when the asset is the wrong kind),
/// and a search picker over the project's matching assets ([`super::AssetCatalog`]) with
/// thumbnails.
fn asset_picker(
    value: &mut String,
    ui: &mut egui::Ui,
    filter: &str,
    id: egui::Id,
    env: InspectorUi<'_>,
) -> bool {
    let name = asset_field_name(env.context);
    let refusal_id = id.with("asset_drop_refused");
    let search_id = id.with("asset_picker_search");
    let mut changed = false;
    // Inline sub-asset editing (ADR-8107): the expand arrow of a data asset, material or physics
    // material shows its fields under the field.
    let inline = asset_rel_path(value).filter(|rel| super::inline_asset::is_inline(rel));
    let mut expanded = false;
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            if let Some(rel) = &inline {
                expanded = super::inline_asset::expand_toggle(ui, rel, &name, id, &mut env.context.pickers);
            }
            if let Some(texture) = env.context.thumbnail(ui.ctx(), value) {
                ui.add(egui::Image::new((
                    texture,
                    egui::vec2(PICKER_THUMB, PICKER_THUMB),
                )));
            }
            let response = ui.add(
                egui::TextEdit::singleline(value)
                    .hint_text(filter)
                    .desired_width(180.0),
            );
            let response = crate::a11y_audit::name(response, egui::WidgetType::TextEdit, &name)
                .on_hover_text(format!("Asset ({filter}); drop one from the Content Browser"));
            if response.changed() {
                changed = true;
                ui.data_mut(|data| data.remove::<String>(refusal_id));
            }
            // A Content Browser drag over the field: green when it would be taken, red when not.
            if let Some(drag) = response.dnd_hover_payload::<crate::dnd::ContentDrag>() {
                let color = match asset_drop_verdict(filter, &drag.path, drag.kind) {
                    Ok(()) => ui.visuals().selection.stroke.color,
                    Err(_) => ui.visuals().error_fg_color,
                };
                ui.painter().rect_stroke(
                    response.rect,
                    2.0,
                    egui::Stroke::new(2.0, color),
                    egui::StrokeKind::Outside,
                );
            }
            if let Some(drag) = response.dnd_release_payload::<crate::dnd::ContentDrag>() {
                match asset_drop_verdict(filter, &drag.path, drag.kind) {
                    Ok(()) => {
                        if *value != drag.path {
                            value.clone_from(&drag.path);
                            changed = true;
                        }
                        ui.data_mut(|data| data.remove::<String>(refusal_id));
                    }
                    Err(reason) => {
                        ui.data_mut(|data| {
                            data.insert_temp(refusal_id, reason);
                        });
                    }
                }
            }
            let assets = std::sync::Arc::clone(&env.context.assets);
            ui.menu_button("\u{1f50d}", |ui| {
                let mut search = ui
                    .data(|data| data.get_temp::<String>(search_id))
                    .unwrap_or_default();
                let field = ui.add(egui::TextEdit::singleline(&mut search).hint_text("search"));
                crate::a11y_audit::name(field, egui::WidgetType::TextEdit, &format!("Search {name}"));
                ui.data_mut(|data| data.insert_temp(search_id, search.clone()));
                let choices = asset_choices(filter, &search, &assets);
                if choices.is_empty() {
                    ui.weak(format!("no {filter} assets"));
                }
                let mut picked = None;
                crate::virtual_list::rows(
                    ui,
                    id.with("asset_picker_rows"),
                    choices.len(),
                    crate::virtual_list::DROP_DOWN_HEIGHT,
                    |ui, i| {
                        let path = choices[i];
                        ui.horizontal(|ui| {
                            match env.context.thumbnail(ui.ctx(), path) {
                                Some(texture) => {
                                    ui.add(egui::Image::new((
                                        texture,
                                        egui::vec2(PICKER_THUMB, PICKER_THUMB),
                                    )));
                                }
                                None => {
                                    ui.add_space(PICKER_THUMB + ui.spacing().item_spacing.x);
                                }
                            }
                            if ui.selectable_label(path == value.as_str(), path).clicked() {
                                picked = Some(path.to_string());
                            }
                        });
                    },
                );
                if let Some(path) = picked {
                    if *value != path {
                        *value = path;
                        changed = true;
                    }
                    ui.data_mut(|data| data.remove::<String>(refusal_id));
                    ui.close();
                }
            })
            .response
            .on_hover_text(format!("Pick a {filter} asset"));
            changed |= asset_field_buttons(value, ui, filter, &name, refusal_id, env.context);
        });
        if let Some(reason) = ui.data(|data| data.get_temp::<String>(refusal_id)) {
            ui.colored_label(ui.visuals().error_fg_color, reason);
        }
        if expanded && let Some(rel) = &inline {
            super::inline_asset::inline_section(ui, rel, &name, id, &mut env.context.pickers);
        }
    });
    changed
}

/// The project-relative path (`assets/...`) an asset field value names; `None` for an empty value
/// or a built-in primitive.
#[must_use]
pub fn asset_rel_path(value: &str) -> Option<String> {
    let value = value.trim().replace('\\', "/");
    if value.is_empty() || value.starts_with("builtin:") {
        return None;
    }
    Some(if value.starts_with("assets/") {
        value
    } else {
        format!("assets/{value}")
    })
}

/// The asset field's buttons (spec addendum § 7.10-7.11, ADR-7786): **Browse to** (reveal in the
/// Content Browser), **Use selected** (the Content Browser's selected asset, when the field takes
/// it), **Clear**, and for an asset other files use too, the shared indicator with the referencer
/// count and **Make unique**. Returns whether the value changed.
fn asset_field_buttons(
    value: &mut String,
    ui: &mut egui::Ui,
    filter: &str,
    name: &str,
    refusal_id: egui::Id,
    context: &mut InspectorContext,
) -> bool {
    let mut changed = false;
    let rel = asset_rel_path(value);
    let browse = ui.add_enabled(rel.is_some(), egui::Button::new("Browse"));
    let browse = crate::a11y_audit::name(browse, egui::WidgetType::Button, &format!("Browse to {name}"))
        .on_hover_text("Show this asset in the Content Browser");
    if browse.clicked()
        && let Some(rel) = &rel
    {
        context.pickers.request(PickerRequest::BrowseTo(rel.clone()));
    }
    let selected = context.pickers.selected_asset.clone();
    let verdict = selected.as_deref().map(|path| {
        asset_drop_verdict(filter, path, crate::dnd::kind_of_path(path)).map(|()| path.to_owned())
    });
    let usable = matches!(verdict, Some(Ok(_)));
    let use_selected = ui.add_enabled(usable, egui::Button::new("Use selected"));
    let hover = match &verdict {
        None => "Select an asset in the Content Browser first".to_owned(),
        Some(Ok(path)) => format!("Use {path}"),
        Some(Err(reason)) => reason.clone(),
    };
    let use_selected = crate::a11y_audit::name(
        use_selected,
        egui::WidgetType::Button,
        &format!("Use selected asset for {name}"),
    )
    .on_hover_text(hover);
    if use_selected.clicked()
        && let Some(Ok(path)) = verdict
        && *value != path
    {
        *value = path;
        changed = true;
        // A button press is its own undo step, never merged with typing in the field.
        context.record(true);
        ui.data_mut(|data| data.remove::<String>(refusal_id));
    }
    let clear = ui.add_enabled(!value.is_empty(), egui::Button::new("Clear"));
    let clear = crate::a11y_audit::name(clear, egui::WidgetType::Button, &format!("Clear {name}"));
    if clear.clicked() && !value.is_empty() {
        value.clear();
        changed = true;
        context.record(true);
    }
    // The shared indicator: counted once per content-index generation, shown from the next frame.
    if let Some(rel) = &rel
        && let Some(count) = context.pickers.referencers_of(rel)
        && count > 1
    {
        let listed = context
            .pickers
            .referencers
            .get(rel)
            .map(|files| {
                let mut shown: Vec<&str> = files.iter().take(8).map(String::as_str).collect();
                if files.len() > shown.len() {
                    shown.push("...");
                }
                shown.join("\n")
            })
            .unwrap_or_default();
        let badge =
            ui.label(egui::RichText::new(format!("\u{1f517} {count}")).color(ui.visuals().warn_fg_color));
        crate::a11y_audit::name(
            badge,
            egui::WidgetType::Label,
            &format!("{name} is shared by {count} files"),
        )
        .on_hover_text(format!("Shared: {count} files use this asset\n{listed}"));
        let unique = ui.button("Make unique");
        let unique =
            crate::a11y_audit::name(unique, egui::WidgetType::Button, &format!("Make {name} unique"))
                .on_hover_text("Copy this asset to a new file next to the level and use the copy here");
        if unique.clicked() {
            context
                .pickers
                .request(PickerRequest::MakeUnique(context.path().to_owned()));
        }
    }
    changed
}

impl InspectorPrimitive for regia_ecs::prelude::Entity {
    fn ui(&mut self, ui: &mut egui::Ui, _: &dyn Any, _: egui::Id, env: InspectorUi<'_>) -> bool {
        entity_field(self, ui, env.context)
    }
}

/// An entity reference field: the entity, an eyedropper (click it, then an entity in the
/// viewport or the Outliner; Esc cancels) and **Clear** (ADR-7786).
fn entity_field(
    value: &mut regia_ecs::prelude::Entity,
    ui: &mut egui::Ui,
    context: &mut InspectorContext,
) -> bool {
    let name = asset_field_name(context);
    let placeholder = *value == regia_ecs::prelude::Entity::PLACEHOLDER;
    let armed = context.pickers.eyedropper.as_deref() == Some(context.path());
    let mut changed = false;
    ui.horizontal(|ui| {
        let shown = if placeholder {
            "(none)".to_owned()
        } else {
            format!("entity {value}")
        };
        ui.label(shown);
        if armed {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Click an entity in the viewport or Outliner (Esc cancels)",
            );
            let cancel = ui.button("Cancel");
            if crate::a11y_audit::name(
                cancel,
                egui::WidgetType::Button,
                &format!("Cancel eyedropper for {name}"),
            )
            .clicked()
            {
                context.pickers.request(PickerRequest::CancelEyedropper);
            }
        } else {
            let pick = ui.button("\u{1f4a7} Pick");
            let pick =
                crate::a11y_audit::name(pick, egui::WidgetType::Button, &format!("Eyedropper for {name}"))
                    .on_hover_text("Then click an entity in the viewport or the Outliner (Esc cancels)");
            if pick.clicked() {
                context
                    .pickers
                    .request(PickerRequest::ArmEyedropper(context.path().to_owned()));
            }
        }
        let clear = ui.add_enabled(!placeholder, egui::Button::new("Clear"));
        if crate::a11y_audit::name(clear, egui::WidgetType::Button, &format!("Clear {name}")).clicked() {
            *value = regia_ecs::prelude::Entity::PLACEHOLDER;
            changed = true;
            context.record(true);
        }
    });
    changed
}

// ---------------------------------------------------------------------------------------------
// glam vectors and matrices

/// The per-component number options of a vector field: explicit `NumberOptions<Vec>` mapped per
/// component, or the field's attribute options.
fn vector_component_options<V: 'static, T: egui::emath::Numeric>(
    options: &dyn Any,
    component: impl Fn(&V) -> T,
) -> (NumberOptions<T>, bool) {
    if let Some(options) = options.downcast_ref::<NumberOptions<V>>() {
        return (options.map(component), false);
    }
    resolve_number_options::<T>(options)
}

/// Linked components (ADR-7785): after lane `edited` went from `before[edited]` to
/// `after[edited]`, the other lanes take the same ratio (uniform scale). From zero there is no
/// ratio, so the other lanes move by the same amount instead.
pub fn link_components(before: &[f64], after: &mut [f64], edited: usize) {
    let (Some(&old), Some(&new)) = (before.get(edited), after.get(edited)) else {
        return;
    };
    for (lane, value) in after.iter_mut().enumerate() {
        if lane == edited {
            continue;
        }
        let Some(&was) = before.get(lane) else {
            continue;
        };
        let next = if old != 0.0 {
            was * (new / old)
        } else {
            was + (new - old)
        };
        if next.is_finite() {
            *value = next;
        }
    }
}

/// The chain toggle of a vector field: linked components scale together. Remembered per field in
/// the user's preferences (`inspector.linked_vectors`).
fn link_toggle(ui: &mut egui::Ui, env: &mut InspectorUi<'_>) {
    let linked = env.context.is_linked();
    let response = ui.selectable_label(linked, "\u{1f517}");
    let enabled = response.enabled();
    let spoken = if linked {
        "Unlink components"
    } else {
        "Link components (scale together)"
    };
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, linked, spoken));
    if response.on_hover_text(spoken).clicked() {
        env.context.toggle_link();
    }
}

macro_rules! vec_ui {
    ($name:ident $ty:ty, $elem:ty: $count:literal $($component:ident)*) => {
        /// Edits the vector's components side by side. A lane that differs across the selection
        /// shows the mixed marker, and editing that lane records only that lane.
        pub fn $name(
            value: &mut dyn Any,
            ui: &mut egui::Ui,
            options: &dyn Any,
            _: egui::Id,
            env: InspectorUi<'_>,
        ) -> bool {
            let Some(value) = value.downcast_mut::<$ty>() else {
                return false;
            };
            let mut changed = false;
            ui.scope(|ui| {
                ui.style_mut().spacing.item_spacing = egui::Vec2::new(4.0, 0.0);
                ui.columns($count, |columns| {
                    if let [$($component),*] = columns {
                        $(
                            let (mut component_options, degrees) =
                                vector_component_options::<$ty, $elem>(options, |v| v.$component);
                            component_options.prefix = concat!(stringify!($component), " ").to_string();
                            mixed_marker($component, env.context.lane_mixed(stringify!($component)));
                            let lane_changed = number_with_options(
                                &mut value.$component,
                                &component_options,
                                degrees,
                                $component,
                            );
                            if lane_changed {
                                env.context.record_lane(stringify!($component));
                            }
                            changed |= lane_changed;
                        )*
                    }
                });
            });
            changed
        }
    };
}

macro_rules! linkable_vec_ui {
    ($name:ident $lanes:ident $ty:ty, $elem:ty: $($component:ident)*) => {
        /// Edits the vector's components side by side behind the chain toggle: when the field's
        /// components are linked, editing one scales the others by the same ratio
        /// ([`link_components`]) and every lane that moved is recorded.
        pub fn $name(
            value: &mut dyn Any,
            ui: &mut egui::Ui,
            options: &dyn Any,
            id: egui::Id,
            mut env: InspectorUi<'_>,
        ) -> bool {
            let Some(vector) = value.downcast_mut::<$ty>() else {
                return false;
            };
            // Matrix columns and other unrecorded drafts draw plain lanes (no toggle).
            if !env.context.recording() {
                return $lanes(vector, ui, options, id, env);
            }
            let before = [$(egui::emath::Numeric::to_f64(vector.$component)),*];
            let mut changed = false;
            ui.horizontal(|ui| {
                link_toggle(ui, &mut env);
                changed = $lanes(&mut *vector, ui, options, id, env.reborrow());
            });
            if changed && env.context.is_linked() {
                let mut after = [$(egui::emath::Numeric::to_f64(vector.$component)),*];
                if let Some(edited) = (0..after.len()).find(|&lane| after[lane] != before[lane]) {
                    link_components(&before, &mut after, edited);
                    let mut lanes = after.iter().zip(before.iter());
                    $(
                        if let Some((&next, &was)) = lanes.next()
                            && next != was
                        {
                            vector.$component = <$elem as egui::emath::Numeric>::from_f64(next);
                            env.context.record_lane(stringify!($component));
                        }
                    )*
                }
            }
            changed
        }
    };
}

macro_rules! bvec_ui {
    ($name:ident $ty:ty: $($component:ident)*) => {
        /// Edits the boolean vector's lanes as checkboxes. A lane that differs shows the mixed
        /// marker, and editing it records only that lane.
        pub fn $name(
            value: &mut dyn Any,
            ui: &mut egui::Ui,
            _: &dyn Any,
            _: egui::Id,
            env: InspectorUi<'_>,
        ) -> bool {
            let Some(value) = value.downcast_mut::<$ty>() else {
                return false;
            };
            let mut changed = false;
            ui.horizontal(|ui| {
                $(
                    mixed_marker(ui, env.context.lane_mixed(stringify!($component)));
                    let lane_changed = ui.checkbox(&mut value.$component, stringify!($component)).changed();
                    if lane_changed {
                        env.context.record_lane(stringify!($component));
                    }
                    changed |= lane_changed;
                )*
            });
            changed
        }
    };
}

macro_rules! mat_ui {
    ($name:ident $ty:ty: $($component:ident)*) => {
        /// Edits the matrix column by column.
        pub fn $name(
            value: &mut dyn Any,
            ui: &mut egui::Ui,
            _: &dyn Any,
            id: egui::Id,
            mut env: InspectorUi<'_>,
        ) -> bool {
            let Some(value) = value.downcast_mut::<$ty>() else {
                return false;
            };
            let mut changed = false;
            mixed_marker(ui, env.context.is_mixed());
            ui.vertical(|ui| {
                // Columns reuse vector widgets, but their lanes are not reflected matrix paths.
                // Let the outer drawer record one whole-matrix edit instead.
                env.suppressed(|env| {
                    $(changed |= env.ui_for_reflect_with_options(&mut value.$component, ui, id.with(stringify!($component)), &());)*
                });
            });
            changed
        }
    };
}

vec_ui!(vec2_lanes Vec2, f32: 2 x y);
vec_ui!(vec3_lanes Vec3, f32: 3 x y z);
vec_ui!(vec3a_lanes Vec3A, f32: 3 x y z);
linkable_vec_ui!(vec2_ui vec2_lanes Vec2, f32: x y);
linkable_vec_ui!(vec3_ui vec3_lanes Vec3, f32: x y z);
linkable_vec_ui!(vec3a_ui vec3a_lanes Vec3A, f32: x y z);
vec_ui!(vec4_ui Vec4, f32: 4 x y z w);
vec_ui!(uvec2_ui UVec2, u32: 2 x y);
vec_ui!(uvec3_ui UVec3, u32: 3 x y z);
vec_ui!(uvec4_ui UVec4, u32: 4 x y z w);
vec_ui!(ivec2_ui IVec2, i32: 2 x y);
vec_ui!(ivec3_ui IVec3, i32: 3 x y z);
vec_ui!(ivec4_ui IVec4, i32: 4 x y z w);
vec_ui!(dvec2_lanes DVec2, f64: 2 x y);
vec_ui!(dvec3_lanes DVec3, f64: 3 x y z);
linkable_vec_ui!(dvec2_ui dvec2_lanes DVec2, f64: x y);
linkable_vec_ui!(dvec3_ui dvec3_lanes DVec3, f64: x y z);
vec_ui!(dvec4_ui DVec4, f64: 4 x y z w);
bvec_ui!(bvec2_ui BVec2: x y);
bvec_ui!(bvec3_ui BVec3: x y z);
bvec_ui!(bvec4_ui BVec4: x y z w);
mat_ui!(mat2_ui Mat2: x_axis y_axis);
mat_ui!(mat3_ui Mat3: x_axis y_axis z_axis);
mat_ui!(mat3a_ui Mat3A: x_axis y_axis z_axis);
mat_ui!(mat4_ui Mat4: x_axis y_axis z_axis w_axis);
mat_ui!(dmat2_ui DMat2: x_axis y_axis);
mat_ui!(dmat3_ui DMat3: x_axis y_axis z_axis);
mat_ui!(dmat4_ui DMat4: x_axis y_axis z_axis w_axis);

// ---------------------------------------------------------------------------------------------
// Quaternions

/// A quaternion's editable form, kept in egui memory between frames so dragging one angle does
/// not make the others jump (Euler extraction is not unique).
trait RotationEdit: Copy + Send + Sync + 'static {
    fn from_quat(quat: Quat) -> Self;
    fn to_quat(self) -> Quat;
    fn ui(&mut self, ui: &mut egui::Ui) -> bool;
}

/// Yaw, pitch, roll (YXZ), edited in degrees.
#[derive(Clone, Copy)]
struct YawPitchRoll((f32, f32, f32));

impl RotationEdit for YawPitchRoll {
    fn from_quat(quat: Quat) -> Self {
        Self(quat.to_euler(EulerRot::YXZ))
    }

    fn to_quat(self) -> Quat {
        let (yaw, pitch, roll) = self.0;
        Quat::from_euler(EulerRot::YXZ, yaw, pitch, roll)
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        let (yaw, pitch, roll) = &mut self.0;
        let mut changed = false;
        egui::Grid::new("ypr grid").show(ui, |ui| {
            for (label, angle) in [("Yaw", yaw), ("Pitch", pitch), ("Roll", roll)] {
                ui.label(label);
                changed |= ui.drag_angle(angle).changed();
                ui.end_row();
            }
        });
        changed
    }
}

/// An axis and an angle.
#[derive(Clone, Copy)]
struct AxisAngle((Vec3, f32));

impl RotationEdit for AxisAngle {
    fn from_quat(quat: Quat) -> Self {
        Self(quat.to_axis_angle())
    }

    fn to_quat(self) -> Quat {
        let (axis, angle) = self.0;
        let axis = axis.normalize();
        if axis.is_nan() {
            Quat::IDENTITY
        } else {
            Quat::from_axis_angle(axis, angle)
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        let (axis, angle) = &mut self.0;
        let mut changed = false;
        egui::Grid::new("axis-angle quat").show(ui, |ui| {
            ui.label("Axis");
            ui.horizontal(|ui| {
                for (i, prefix) in ["x ", "y ", "z "].into_iter().enumerate() {
                    changed |= ui
                        .add(egui::DragValue::new(&mut axis[i]).speed(0.01).prefix(prefix))
                        .changed();
                }
            });
            ui.end_row();
            ui.label("Angle");
            changed |= ui.drag_angle(angle).changed();
            ui.end_row();
        });
        changed
    }
}

fn quat_ui_kind<T: RotationEdit>(value: &mut Quat, ui: &mut egui::Ui) -> bool {
    let id = ui.id();
    let mut intermediate = ui.memory_mut(|memory| {
        *memory
            .data
            .get_temp_mut_or_insert_with(id, || T::from_quat(*value))
    });
    let externally_changed = !intermediate.to_quat().abs_diff_eq(*value, 1e-5)
        && !intermediate.to_quat().abs_diff_eq(-*value, 1e-5);
    if externally_changed {
        intermediate = T::from_quat(*value);
    }
    let mut changed = intermediate.ui(ui);
    if changed {
        let edited = intermediate.to_quat();
        if edited.is_finite() {
            *value = edited;
        } else {
            // Typed `inf`/`nan`/huge angles make no rotation (ADR-3290): keep the old one.
            intermediate = T::from_quat(*value);
            changed = false;
        }
    }
    if changed || externally_changed {
        ui.memory_mut(|memory| memory.data.insert_temp(id, intermediate));
    }
    changed
}

/// Euler degrees, one drag per axis. A mixed axis shows the marker, and a changed axis is
/// recorded on its own (`rx` / `ry` / `rz`) so the other axes stay per entity.
fn euler_axes_ui(value: &mut Quat, ui: &mut egui::Ui, env: &mut InspectorUi<'_>) -> bool {
    let id = ui.id();
    let mut euler = ui.memory_mut(|memory| {
        *memory
            .data
            .get_temp_mut_or_insert_with(id, || super::quat_to_euler_deg(value))
    });
    let current = super::euler_deg_to_quat(euler);
    let externally_changed = !current.abs_diff_eq(*value, 1e-5) && !current.abs_diff_eq(-*value, 1e-5);
    if externally_changed {
        euler = super::quat_to_euler_deg(value);
    }
    let mut changed = false;
    let mut touched = [false; 3];
    let lanes = ["rx", "ry", "rz"];
    ui.horizontal(|ui| {
        for (index, lane) in lanes.iter().enumerate() {
            mixed_marker(ui, env.context.lane_mixed(lane));
            let before = euler[index];
            let lane_changed = ui
                .add(
                    egui::DragValue::new(&mut euler[index])
                        .speed(1.0)
                        .prefix(format!("{lane} "))
                        .suffix("°"),
                )
                .changed();
            // The raw drag accepts `inf` / `nan` / `1e40`. Yaw-pitch-roll and axis-angle already
            // drop those (ADR-3290); this is the default Euler view and used to write them.
            if !euler[index].is_finite() {
                euler[index] = before;
                continue;
            }
            if lane_changed && euler[index] != before {
                touched[index] = true;
                changed = true;
            }
        }
    });
    if changed {
        let edited = super::euler_deg_to_quat(euler);
        if edited.is_finite() {
            *value = edited;
            for (index, lane) in lanes.iter().enumerate() {
                if touched[index] {
                    env.context.record_lane_value(lane, euler[index]);
                }
            }
        } else {
            euler = super::quat_to_euler_deg(value);
            changed = false;
        }
    }
    if changed || externally_changed {
        ui.memory_mut(|memory| memory.data.insert_temp(id, euler));
    }
    changed
}

/// Edits a quaternion as Euler degrees (default), yaw/pitch/roll, axis-angle or raw.
pub fn quat_ui(
    value: &mut dyn Any,
    ui: &mut egui::Ui,
    options: &dyn Any,
    _: egui::Id,
    mut env: InspectorUi<'_>,
) -> bool {
    let Some(value) = value.downcast_mut::<Quat>() else {
        return false;
    };
    let options = options.downcast_ref::<QuatOptions>().copied().unwrap_or_default();
    ui.vertical(|ui| match options.display {
        QuatDisplay::Raw => {
            let mut raw = Vec4::from(*value);
            let mut changed = false;
            ui.horizontal(|ui| {
                for (i, prefix) in ["x ", "y ", "z ", "w "].into_iter().enumerate() {
                    changed |= ui
                        .add(egui::DragValue::new(&mut raw[i]).speed(0.01).prefix(prefix))
                        .changed();
                }
            });
            // A zero or non-finite vector has no rotation to normalize to (ADR-3290).
            let edited = Quat::from_vec4(raw).normalize();
            if changed && edited.is_finite() {
                *value = edited;
                true
            } else {
                false
            }
        }
        QuatDisplay::Euler => euler_axes_ui(value, ui, &mut env),
        QuatDisplay::YawPitchRoll => quat_ui_kind::<YawPitchRoll>(value, ui),
        QuatDisplay::AxisAngle => quat_ui_kind::<AxisAngle>(value, ui),
    })
    .inner
}

// ---------------------------------------------------------------------------------------------
// Colours (Regia Engine)

/// Edits a linear-RGB(A) colour with the sRGB picker: `[f32; 3]`/`[f32; 4]` always, `Vec3`/`Vec4`
/// when the field is colour-like. HDR values (above 1) clamp in the picker; with `hdr` (or when
/// a channel already exceeds 1) the numeric channels are shown too so they stay reachable.
/// Returns `None` when `value` is not a colour type.
pub fn color_ui(
    value: &mut dyn PartialReflect,
    ui: &mut egui::Ui,
    hdr: bool,
    vectors_too: bool,
) -> Option<bool> {
    color_ui_in(value, ui, hdr, vectors_too, None)
}

/// The color field's eyedropper button: arms it (then click a pixel anywhere in the window, or
/// move with the arrow keys and press Enter; Esc cancels), or while armed cancels it.
fn color_eyedropper_button(ui: &mut egui::Ui, context: &mut InspectorContext, path: &str, name: &str) {
    let armed = context.pickers.color_eyedropper.as_deref() == Some(path);
    let response = ui.selectable_label(armed, "\u{1f4a7}");
    let spoken = if armed {
        format!("Cancel the {name} eyedropper")
    } else {
        format!("{name} eyedropper")
    };
    let response = crate::a11y_audit::name(response, egui::WidgetType::Button, &spoken);
    if response.clicked() {
        context.pickers.request(if armed {
            PickerRequest::CancelColorEyedropper
        } else {
            PickerRequest::ArmColorEyedropper(path.to_owned())
        });
    }
}

/// [`color_ui`] with the multi-select mask. A picker change records the whole colour. A numeric
/// channel records only that channel (`[i]` for arrays, `.x`/`.y`/`.z`/`.w` for vectors).
pub(crate) fn color_ui_in(
    value: &mut dyn PartialReflect,
    ui: &mut egui::Ui,
    hdr: bool,
    vectors_too: bool,
    mut context: Option<&mut InspectorContext>,
) -> Option<bool> {
    let is_vec = value.try_downcast_ref::<Vec3>().is_some() || value.try_downcast_ref::<Vec4>().is_some();
    let (mut rgba, alpha) = if let Some(c) = value.try_downcast_ref::<[f32; 3]>() {
        ([c[0], c[1], c[2], 1.0], false)
    } else if let Some(c) = value.try_downcast_ref::<[f32; 4]>() {
        (*c, true)
    } else if vectors_too && let Some(v) = value.try_downcast_ref::<Vec3>() {
        ([v.x, v.y, v.z, 1.0], false)
    } else if vectors_too && let Some(v) = value.try_downcast_ref::<Vec4>() {
        (v.to_array(), true)
    } else {
        return None;
    };
    let show_numbers = hdr || rgba[..3].iter().any(|c| *c > 1.0);
    let mut picker_changed = false;
    let mut channel_changed = [false; 4];
    // The full picker (ADR-7786): a swatch opens it below the row.
    let path = context
        .as_ref()
        .map(|ctx| ctx.path().to_owned())
        .unwrap_or_default();
    let picker_id = ui.id().with(("color_picker", path.as_str()));
    let name = match path.rsplit_once('.') {
        Some((_, last)) => last.to_owned(),
        None if path.is_empty() => "Color".to_owned(),
        None => path.clone(),
    };
    let outcome = ui
        .vertical(|ui| {
            ui.horizontal(|ui| {
                mixed_marker(ui, context.as_ref().is_some_and(|ctx| ctx.is_mixed()));
                super::color::picker_toggle(ui, picker_id, rgba, &name);
                // The eyedropper (P-133, ADR-8106): pick a pixel anywhere in the window.
                if let Some(ctx) = context.as_mut()
                    && !path.is_empty()
                {
                    color_eyedropper_button(ui, ctx, &path, &name);
                }
                if show_numbers {
                    let channels = if alpha { 4 } else { 3 };
                    let lanes = ["x", "y", "z", "w"];
                    for (index, (channel, prefix)) in rgba
                        .iter_mut()
                        .take(channels)
                        .zip(["r ", "g ", "b ", "a "])
                        .enumerate()
                    {
                        let mixed = context.as_ref().is_some_and(|ctx| {
                            if is_vec {
                                ctx.lane_mixed(lanes[index])
                            } else {
                                ctx.index_mixed(index)
                            }
                        });
                        mixed_marker(ui, mixed);
                        let before = *channel;
                        let edited = ui
                            .add(
                                egui::DragValue::new(channel)
                                    .speed(0.01)
                                    .range(0.0..=f32::MAX)
                                    .prefix(prefix),
                            )
                            .changed();
                        if edited && *channel != before {
                            channel_changed[index] = true;
                        }
                    }
                }
            });
            super::color::picker_panel(
                ui,
                picker_id,
                &mut rgba,
                alpha,
                hdr,
                &name,
                context.as_mut().map(|ctx| &mut ctx.pickers),
            )
        })
        .inner;
    if outcome.changed {
        picker_changed = true;
        channel_changed = [false; 4];
    }
    if outcome.committed
        && let Some(ctx) = context.as_mut()
    {
        ctx.pickers
            .request(super::picker_links::PickerRequest::Recent(rgba));
    }
    let any_channel = channel_changed.iter().any(|changed| *changed);
    if picker_changed || any_channel {
        if let Some(c) = value.try_downcast_mut::<[f32; 3]>() {
            *c = [rgba[0], rgba[1], rgba[2]];
        } else if let Some(c) = value.try_downcast_mut::<[f32; 4]>() {
            *c = rgba;
        } else if let Some(v) = value.try_downcast_mut::<Vec3>() {
            *v = Vec3::new(rgba[0], rgba[1], rgba[2]);
        } else if let Some(v) = value.try_downcast_mut::<Vec4>() {
            *v = Vec4::from_array(rgba);
        }
    }
    // A picker edit replaces the colour. A numeric channel edit records only that channel, so
    // the other channels stay per entity.
    if !picker_changed && let Some(ctx) = context.as_mut() {
        let lanes = ["x", "y", "z", "w"];
        for (index, changed) in channel_changed.iter().enumerate() {
            if *changed {
                if is_vec {
                    ctx.record_lane(lanes[index]);
                } else {
                    ctx.record_index(index);
                }
            }
        }
    }
    Some(picker_changed || any_channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_filters_match_globs_and_kinds() {
        assert!(asset_matches("*.ogg", "assets/sfx/door.OGG", ContentKind::Other));
        assert!(!asset_matches("*.ogg", "assets/sfx/door.wav", ContentKind::Other));
        assert!(asset_matches("mesh", "assets/rock.glb", ContentKind::Mesh));
        assert!(!asset_matches("mesh", "assets/rock.png", ContentKind::Other));
        assert!(asset_matches(
            "texture, audio",
            "assets/a.wav",
            ContentKind::Other
        ));
        assert!(asset_matches(
            "material",
            "assets/m/rock.material.ron",
            ContentKind::Other
        ));
        assert!(asset_matches(
            "prefab",
            "assets/crate.prefab.ron",
            ContentKind::Prefab
        ));
        assert!(asset_matches("png", "assets/t.png", ContentKind::Other));
        assert!(!asset_matches("", "assets/t.png", ContentKind::Other));
    }

    #[test]
    fn the_builtin_table_covers_std_and_glam_leaves() {
        let impls = InspectorImpls::global();
        for type_id in [
            TypeId::of::<f32>(),
            TypeId::of::<u8>(),
            TypeId::of::<bool>(),
            TypeId::of::<String>(),
            TypeId::of::<Duration>(),
            TypeId::of::<std::ops::RangeInclusive<f32>>(),
            TypeId::of::<Vec3>(),
            TypeId::of::<IVec2>(),
            TypeId::of::<BVec3>(),
            TypeId::of::<Mat4>(),
            TypeId::of::<Quat>(),
        ] {
            assert!(impls.get(type_id).is_some(), "{type_id:?}");
        }
        assert!(
            impls.get(TypeId::of::<Vec<f32>>()).is_none(),
            "containers use the reflect drawer"
        );
    }

    #[test]
    fn rotation_edits_round_trip() {
        let quat = Quat::from_euler(EulerRot::XYZ, 0.3, -0.4, 1.1);
        for back in [
            super::super::euler_deg_to_quat(super::super::quat_to_euler_deg(&quat)),
            YawPitchRoll::from_quat(quat).to_quat(),
            AxisAngle::from_quat(quat).to_quat(),
        ] {
            assert!(
                back.abs_diff_eq(quat, 1e-4) || back.abs_diff_eq(-quat, 1e-4),
                "{back:?} vs {quat:?}"
            );
        }
    }

    /// Types `text` into a fresh drag value holding 1.0, the way a user does: click it (it turns
    /// into a text field with its text selected), type, press Enter. Returns the value after.
    fn type_into_number(text: &str, degrees: bool) -> f32 {
        let ctx = egui::Context::default();
        let mut value = 1.0_f32;
        let options = NumberOptions::<f32>::default();
        let mut rect = egui::Rect::NOTHING;
        let frame = |events: Vec<egui::Event>, value: &mut f32, rect: &mut egui::Rect| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 200.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                let _ = number_with_options(value, &options, degrees, ui);
                *rect = ui.min_rect();
            });
            output.textures_delta.clear();
        };
        frame(Vec::new(), &mut value, &mut rect);
        let at = rect.left_center() + egui::vec2(12.0, 0.0);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(
            vec![egui::Event::PointerMoved(at), button(true)],
            &mut value,
            &mut rect,
        );
        frame(vec![button(false)], &mut value, &mut rect);
        frame(Vec::new(), &mut value, &mut rect);
        frame(vec![egui::Event::Text(text.to_owned())], &mut value, &mut rect);
        let enter = |pressed| egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![enter(true), enter(false)], &mut value, &mut rect);
        frame(Vec::new(), &mut value, &mut rect);
        value
    }

    /// Types `text` into the default Euler `rx` drag the way a user does.
    fn type_into_euler_rx(text: &str) -> Quat {
        let ctx = egui::Context::default();
        let mut value = Quat::from_rotation_y(0.4);
        let options = QuatOptions::default();
        let registry = regia_ecs::reflect::TypeRegistry::new();
        let mut context = InspectorContext::default();
        let mut rect = egui::Rect::NOTHING;
        let mut frame = |events: Vec<egui::Event>, value: &mut Quat, rect: &mut egui::Rect| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 200.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                let env = InspectorUi::new(&registry, &mut context);
                let _ = quat_ui(value, ui, &options, ui.id(), env);
                *rect = ui.min_rect();
            });
            output.textures_delta.clear();
        };
        frame(Vec::new(), &mut value, &mut rect);
        let at = rect.left_center() + egui::vec2(12.0, 0.0);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(
            vec![egui::Event::PointerMoved(at), button(true)],
            &mut value,
            &mut rect,
        );
        frame(vec![button(false)], &mut value, &mut rect);
        frame(Vec::new(), &mut value, &mut rect);
        frame(vec![egui::Event::Text(text.to_owned())], &mut value, &mut rect);
        let enter = |pressed| egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![enter(true), enter(false)], &mut value, &mut rect);
        frame(Vec::new(), &mut value, &mut rect);
        value
    }

    /// The default rotation widget is Euler degrees. Typing `inf` into `rx` used to store a
    /// non-finite quaternion; yaw/pitch/roll already refused that.
    #[test]
    fn typed_non_finite_euler_keeps_the_old_rotation() {
        let start = Quat::from_rotation_y(0.4);
        let turned = type_into_euler_rx("45");
        assert!(
            turned.is_finite() && !turned.abs_diff_eq(start, 1e-4),
            "typing 45 must change rx, got {turned:?}"
        );
        for typed in ["inf", "-inf", "nan", "1e40", "-1e40"] {
            let after = type_into_euler_rx(typed);
            assert!(
                after.is_finite() && after.abs_diff_eq(start, 1e-4),
                "{typed:?} gave {after:?}"
            );
        }
    }

    /// Monkey seeds 0x3, 0x7 and 0xb (ADR-3290): `-inf` or `1e40` typed into an Inspector
    /// number made a non-finite transform. A non-finite result keeps the old value.
    #[test]
    fn typed_non_finite_numbers_keep_the_old_value() {
        // The typing path works (a finite number goes in), so the checks below are not vacuous.
        assert_eq!(type_into_number("2.5", false), 2.5);
        // `1e40` overflows f32 as a plain number; as degrees it is a finite (if silly) angle.
        for (degrees, inputs) in [
            (false, &["inf", "-inf", "nan", "1e40", "-1e40"][..]),
            (true, &["inf", "-inf", "nan", "1e400"][..]),
        ] {
            for typed in inputs {
                let after = type_into_number(typed, degrees);
                // Degrees round-trip through the shown text, so allow its rounding.
                assert!(
                    (after - 1.0).abs() < 1e-3,
                    "{typed:?} (degrees {degrees}) gave {after}"
                );
            }
        }
    }
}
