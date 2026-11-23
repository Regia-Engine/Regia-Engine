// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from bevy-inspector-egui 0.37.0
// (https://github.com/jakobhellermann/bevy-inspector-egui, commit ac67298,
// crates/bevy-inspector-egui/src/inspector_options/{mod,std_options}.rs),
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

//! Per-field display options (`specs/19` § 2 "per-type custom drawers").
//!
//! Upstream attaches options with a derive macro (`#[inspector(min = .., max = ..)]`). Regia Engine
//! drives them from the editor attributes components already carry
//! (`#[reflect(@Range(0.0..=1.0), @Step(0.05), @Units("m"))]`, [`regia_ecs::attrs`]) plus the
//! inspector's name conventions ([`field_unit`], [`is_color_name`]),
//! so runtime crates need no editor dependency. Explicit [`InspectorOptions`] can still be
//! registered as [`ReflectInspectorOptions`] type data and win over the derived ones.

use std::any::Any;
use std::collections::HashMap;

use regia_ecs::attrs::EditorAttrs;
use regia_ecs::reflect::TypeData;

use super::{FieldUnit, field_unit, is_color_name};

/// A path into a struct or enum: a field (`.foo`) or a variant's field (`Some.0`).
#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
pub enum Target {
    /// The `n`th field of a struct, tuple struct or tuple.
    Field(usize),
    /// A field of one enum variant.
    VariantField {
        /// Index of the variant.
        variant_index: usize,
        /// Index of the field within the variant.
        field_index: usize,
    },
}

/// Options for the fields of one struct or enum, keyed by [`Target`]. The values are arbitrary
/// type data read by the leaf widgets ([`NumberOptions`], [`QuatOptions`], [`FieldOptions`]).
#[derive(Default)]
pub struct InspectorOptions {
    options: HashMap<Target, Box<dyn TypeData>>,
}

impl std::fmt::Debug for InspectorOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut options = f.debug_struct("InspectorOptions");
        for entry in self.options.keys() {
            options.field(&format!("{entry:?}"), &"..");
        }
        options.finish()
    }
}

impl Clone for InspectorOptions {
    fn clone(&self) -> Self {
        Self {
            options: self
                .options
                .iter()
                .map(|(target, data)| (*target, TypeData::clone_type_data(&**data)))
                .collect(),
        }
    }
}

impl InspectorOptions {
    /// No options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the options for one target.
    pub fn insert<T: TypeData>(&mut self, target: Target, options: T) {
        self.options.insert(target, Box::new(options));
    }

    /// Sets already-boxed options for one target.
    pub fn insert_boxed(&mut self, target: Target, options: Box<dyn TypeData>) {
        self.options.insert(target, options);
    }

    /// The options for one target.
    #[must_use]
    pub fn get(&self, target: Target) -> Option<&dyn Any> {
        self.options.get(&target).map(|value| value.as_any())
    }

    /// Every `(target, options)` pair.
    pub fn iter(&self) -> impl Iterator<Item = (Target, &dyn TypeData)> + '_ {
        self.options.iter().map(|(target, data)| (*target, &**data))
    }
}

/// [`InspectorOptions`] stored as type data in the type registry
/// (`registry.get_mut(type_id).insert(ReflectInspectorOptions(options))`).
#[derive(Clone, Debug)]
pub struct ReflectInspectorOptions(pub InspectorOptions);

/// Options for numeric fields.
#[derive(Clone, Debug)]
pub struct NumberOptions<T> {
    /// Lower clamp.
    pub min: Option<T>,
    /// Upper clamp.
    pub max: Option<T>,
    /// Drag speed per pixel (`0` = the widget's default).
    pub speed: f32,
    /// Text before the number.
    pub prefix: String,
    /// Text after the number (units).
    pub suffix: String,
    /// Drag value or slider.
    pub display: NumberDisplay,
}

impl<T> Default for NumberOptions<T> {
    fn default() -> Self {
        Self {
            min: None,
            max: None,
            speed: 0.0,
            prefix: String::new(),
            suffix: String::new(),
            display: NumberDisplay::default(),
        }
    }
}

/// How a number is edited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NumberDisplay {
    /// A drag value (default).
    #[default]
    Drag,
    /// A slider between `min` and `max`.
    Slider,
}

impl<T> NumberOptions<T> {
    /// Clamped to `min..=max`.
    pub fn between(min: T, max: T) -> Self {
        Self {
            min: Some(min),
            max: Some(max),
            ..Self::default()
        }
    }

    /// Clamped to `min..`.
    pub fn at_least(min: T) -> Self {
        Self {
            min: Some(min),
            ..Self::default()
        }
    }

    /// With a drag speed.
    #[must_use]
    pub fn with_speed(self, speed: f32) -> Self {
        Self { speed, ..self }
    }

    /// Maps the bounds to another number type (vector options → per-component options).
    pub fn map<U>(&self, f: impl Fn(&T) -> U) -> NumberOptions<U> {
        NumberOptions {
            min: self.min.as_ref().map(&f),
            max: self.max.as_ref().map(&f),
            speed: self.speed,
            prefix: self.prefix.clone(),
            suffix: self.suffix.clone(),
            display: self.display,
        }
    }
}

impl<T: egui::emath::Numeric> NumberOptions<T> {
    /// Clamped to `0..`.
    #[must_use]
    pub fn positive() -> Self {
        Self::at_least(T::from_f64(0.0))
    }

    /// Clamped to `0..=1` with a fine drag speed.
    #[must_use]
    pub fn normalized() -> Self {
        Self::between(T::from_f64(0.0), T::from_f64(1.0)).with_speed(0.01)
    }
}

/// How a quaternion is edited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuatDisplay {
    /// The four raw components (renormalised after the edit).
    Raw,
    /// Euler angles in degrees, XYZ (Regia Engine's default; matches the gizmo readout).
    #[default]
    Euler,
    /// Yaw, pitch, roll in degrees (YXZ).
    YawPitchRoll,
    /// An axis and an angle in degrees.
    AxisAngle,
}

/// Options for quaternion fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuatOptions {
    /// How the rotation is shown.
    pub display: QuatDisplay,
}

/// Regia Engine's per-field options: the field's editor attributes ([`EditorAttrs`]) plus the
/// inspector's name conventions. Leaf widgets read it like upstream's typed options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldOptions {
    /// The field's editor attributes.
    pub attrs: EditorAttrs,
    /// The unit inferred from the field and component names ([`field_unit`]).
    pub unit: FieldUnit,
    /// Whether the field is a colour (a colour-like name, or `@ColorHdr`).
    pub color: bool,
}

impl FieldOptions {
    /// Options for the field `field` of a value inside the component `component` (type path).
    #[must_use]
    pub fn for_field(component: &str, field: &str, attrs: EditorAttrs) -> Self {
        let unit = field_unit(component, field);
        let color = attrs.color_hdr || is_color_name(field);
        Self { attrs, unit, color }
    }

    /// Whether the stored value is radians edited as degrees: a name-inferred angle without an
    /// explicit `@Units` (an explicit unit means the value is stored in that unit).
    #[must_use]
    pub fn radians_as_degrees(&self) -> bool {
        self.unit == FieldUnit::Degrees && self.attrs.units.is_none()
    }

    /// The suffix shown after numbers.
    #[must_use]
    pub fn suffix(&self) -> String {
        match self.attrs.units {
            Some(units) => format!(" {units}"),
            None => self.unit.suffix().to_string(),
        }
    }

    /// Number options for a field of numeric type `T`, in the *displayed* unit (degrees for
    /// radian angles).
    #[must_use]
    pub fn number_options<T: egui::emath::Numeric>(&self) -> NumberOptions<T> {
        let to_display = |v: f64| {
            if self.radians_as_degrees() {
                v.to_degrees()
            } else {
                v
            }
        };
        let (min, max) = match &self.attrs.range {
            Some(range) => (
                Some(T::from_f64(to_display(*range.start()))),
                Some(T::from_f64(to_display(*range.end()))),
            ),
            None => (None, None),
        };
        let speed = match self.attrs.step {
            Some(step) => to_display(step) as f32,
            None if self.radians_as_degrees() => 1.0,
            None => 0.0,
        };
        NumberOptions {
            min,
            max,
            speed,
            prefix: String::new(),
            suffix: self.suffix(),
            display: NumberDisplay::Drag,
        }
    }
}

/// The options for field `field` out of a parent's options: explicit [`InspectorOptions`] first,
/// then `fallback` (the attribute-derived [`FieldOptions`]).
pub(crate) fn struct_field_options<'a>(
    options: &'a dyn Any,
    field: usize,
    fallback: &'a dyn Any,
) -> &'a dyn Any {
    options
        .downcast_ref::<InspectorOptions>()
        .and_then(|options| options.get(Target::Field(field)))
        .unwrap_or(fallback)
}

/// The options for a variant field out of a parent's options (see [`struct_field_options`]).
pub(crate) fn variant_field_options<'a>(
    options: &'a dyn Any,
    variant_index: usize,
    field_index: usize,
    fallback: &'a dyn Any,
) -> &'a dyn Any {
    options
        .downcast_ref::<InspectorOptions>()
        .and_then(|options| {
            options.get(Target::VariantField {
                variant_index,
                field_index,
            })
        })
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_options_become_number_options() {
        let attrs = EditorAttrs {
            range: Some(0.0..=10.0),
            step: Some(0.5),
            units: Some("m/s"),
            ..EditorAttrs::default()
        };
        let options = FieldOptions::for_field("game::Mover", "top_speed", attrs);
        let number = options.number_options::<f32>();
        assert_eq!(number.min, Some(0.0));
        assert_eq!(number.max, Some(10.0));
        assert_eq!(number.speed, 0.5);
        assert_eq!(number.suffix, " m/s");
    }

    #[test]
    fn radian_angles_display_in_degrees() {
        let attrs = EditorAttrs {
            range: Some(0.0..=std::f64::consts::PI),
            ..EditorAttrs::default()
        };
        let options = FieldOptions::for_field("regia_render::camera::Camera", "fov_y", attrs);
        assert!(options.radians_as_degrees());
        let number = options.number_options::<f64>();
        assert!((number.max.unwrap_or_default() - 180.0).abs() < 1e-9);
        assert_eq!(number.suffix, "°");
        // An explicit unit means the value is stored in that unit.
        let explicit = FieldOptions::for_field(
            "game::Door",
            "open_angle",
            EditorAttrs {
                units: Some("deg"),
                ..EditorAttrs::default()
            },
        );
        assert!(!explicit.radians_as_degrees());
        assert_eq!(explicit.suffix(), " deg");
    }

    #[test]
    fn explicit_options_win_over_derived() {
        let mut explicit = InspectorOptions::new();
        explicit.insert(Target::Field(1), NumberOptions::<f32>::normalized());
        let fallback = FieldOptions::default();
        let field = struct_field_options(&explicit, 1, &fallback);
        assert!(field.is::<NumberOptions<f32>>());
        let other = struct_field_options(&explicit, 0, &fallback);
        assert!(other.is::<FieldOptions>());
        let variant = variant_field_options(&(), 1, 0, &fallback);
        assert!(variant.is::<FieldOptions>());
    }
}
