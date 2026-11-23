// SPDX-License-Identifier: MPL-2.0
//! Editor attributes (`docs/specs/10-core.md` § 2.2).
//!
//! Attach them to fields (or types and variants) with bevy_reflect's custom attribute syntax:
//!
//! ```
//! use regia_ecs::attrs::{Category, Range, Tooltip, Units};
//! use regia_ecs::prelude::*;
//!
//! #[derive(Reflect, Default)]
//! #[reflect(Default)]
//! struct Door {
//!     #[reflect(@Range(0.0..=180.0), @Units("deg"), @Tooltip("Fully open angle"))]
//!     open_angle: f32,
//!     #[reflect(@Category("Audio"))]
//!     creak_volume: f32,
//! }
//!
//! let attrs = regia_ecs::describe::field_attrs::<Door>("open_angle").unwrap_or_default();
//! assert_eq!(attrs.range, Some(0.0..=180.0));
//! assert_eq!(attrs.units, Some("deg"));
//! ```
//!
//! The inspector and the agent protocol read them through [`EditorAttrs`]; the runtime ignores
//! them. Every attribute is a plain reflected value, so unknown (game-defined) attributes are also
//! reachable through [`CustomAttributes::iter`].

use core::ops::RangeInclusive;

use bevy_reflect::Reflect;
use bevy_reflect::attributes::CustomAttributes;

/// Inclusive numeric range of a field: sliders clamp to it, `types.describe` emits `minimum`/`maximum`.
#[derive(Reflect, Debug, Clone, PartialEq)]
pub struct Range(pub RangeInclusive<f64>);

impl Range {
    /// The range `min..=max`.
    pub fn new(min: f64, max: f64) -> Self {
        Self(min..=max)
    }
}

/// Drag/step increment for a numeric field.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
pub struct Step(pub f64);

/// Hover help text.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tooltip(pub &'static str);

/// Label shown instead of the field name.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayName(pub &'static str);

/// Category label of the field; the Inspector groups matching fields under a collapsible header.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Category(pub &'static str);

/// Not shown in the inspector (still serialized).
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hidden;

/// Shown but not editable.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadOnly;

/// String edited in a multi-line text box.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Multiline;

/// Color edited as HDR (components may exceed 1.0).
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColorHdr;

/// Asset picker filter, e.g. `"mesh"` or `"*.ogg"`.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetFilter(pub &'static str);

/// Display units: `"m"`, `"deg"`, `"s"`, `"dB"`, ...
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Units(pub &'static str);

/// Greys a field out in the inspector while the condition over its sibling fields is false
/// (addendum § 7.5). Grammar: clauses joined by `&&`, each `[!]path [op value]` where `path` is a
/// sibling field (dots reach into it: `active_events.contact_force`), `op` is one of
/// `== != < <= > >=` and `value` a number, `true`/`false`, an enum variant name or a `"string"`.
/// A clause without `op` is true for a `true` bool, a `Some` option, a non-zero number or a
/// non-empty string. Examples: `"cast_shadows"`, `"kind == Spot"`, `"enabled && weight > 0"`.
/// A condition that does not parse or names a missing field never greys the field.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditCondition(pub &'static str);

/// With [`EditCondition`]: hide the field instead of greying it while the condition is false.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditConditionHides;

/// On a field of a struct type with an `enabled: bool` field: the inspector draws `enabled` as a
/// checkbox in the field's header and greys the body while it is off (addendum § 7.5). `Option`
/// fields get the same header checkbox without the attribute.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Toggle;

/// An integer field (or a newtype over one) holding bit flags: the inspector draws one checkbox
/// per named bit. The names are separated by commas, bit 0 first; an empty name skips that bit
/// (`"locked,,alarmed"` names bits 0 and 2).
///
/// ```
/// # use regia_ecs::attrs::Flags;
/// # use regia_ecs::prelude::*;
/// #[derive(Reflect, Default)]
/// struct Door {
///     #[reflect(@Flags("locked, jammed, alarmed"))]
///     state: u8,
/// }
/// let attrs = regia_ecs::describe::field_attrs::<Door>("state").unwrap_or_default();
/// assert_eq!(attrs.flag_names(), vec![(0, "locked"), (1, "jammed"), (2, "alarmed")]);
/// ```
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags(pub &'static str);

/// An integer bit mask over named layers: the inspector draws the bit-layer grid. The domain
/// names where the layer names come from: `"physics"` reads the project's collision layers
/// (`config/physics.ron`); any other domain shows numbered layers.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerMask(pub &'static str);

/// A string or path field holding a project file: the inspector adds a browse button with a
/// picker confined to the project. The filter takes globs separated by `;` or `,`
/// (`"*.png;*.jpg"`); an empty filter takes every file.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilePicker(pub &'static str);

/// A string or path field holding a project folder: the inspector adds a browse button with a
/// folder picker confined to the project.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirPicker;

/// The field is a button with this label instead of a value: clicking it runs the component
/// action named after the field, which the game registers in the editor (the field itself is
/// usually a unit-like marker such as `()`). The runtime ignores it.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Button(pub &'static str);

/// The Regia Engine editor attributes found on a field, type or variant, in plain form.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditorAttrs {
    /// [`Range`]; `None` when the attribute is absent or malformed (non-finite endpoint or
    /// `start > end`), so consumers never see a reversed or unbounded range.
    pub range: Option<RangeInclusive<f64>>,
    /// [`Step`]; `None` when absent or malformed (non-finite or not greater than zero).
    pub step: Option<f64>,
    /// [`Tooltip`].
    pub tooltip: Option<&'static str>,
    /// [`DisplayName`].
    pub display_name: Option<&'static str>,
    /// [`Category`].
    pub category: Option<&'static str>,
    /// [`Hidden`].
    pub hidden: bool,
    /// [`ReadOnly`].
    pub read_only: bool,
    /// [`Multiline`].
    pub multiline: bool,
    /// [`ColorHdr`].
    pub color_hdr: bool,
    /// [`AssetFilter`].
    pub asset_filter: Option<&'static str>,
    /// [`Units`].
    pub units: Option<&'static str>,
    /// [`EditCondition`]: the condition source text.
    pub edit_condition: Option<&'static str>,
    /// [`EditConditionHides`].
    pub edit_condition_hides: bool,
    /// [`Toggle`].
    pub toggle: bool,
    /// [`Flags`]: the comma-separated bit names ([`Self::flag_names`]).
    pub flags: Option<&'static str>,
    /// [`LayerMask`]: the layer domain.
    pub layer_mask: Option<&'static str>,
    /// [`FilePicker`]: the file filter.
    pub file_picker: Option<&'static str>,
    /// [`DirPicker`].
    pub dir_picker: bool,
    /// [`Button`]: the button label.
    pub button: Option<&'static str>,
}

impl EditorAttrs {
    /// Reads the Regia Engine attributes out of a custom attribute collection.
    pub fn from_custom(attrs: &CustomAttributes) -> Self {
        Self {
            range: attrs
                .get::<Range>()
                .map(|r| r.0.clone())
                .filter(|r| r.start().is_finite() && r.end().is_finite() && r.start() <= r.end()),
            step: attrs
                .get::<Step>()
                .map(|s| s.0)
                .filter(|s| s.is_finite() && *s > 0.0),
            tooltip: attrs.get::<Tooltip>().map(|t| t.0),
            display_name: attrs.get::<DisplayName>().map(|d| d.0),
            category: attrs.get::<Category>().map(|c| c.0),
            hidden: attrs.contains::<Hidden>(),
            read_only: attrs.contains::<ReadOnly>(),
            multiline: attrs.contains::<Multiline>(),
            color_hdr: attrs.contains::<ColorHdr>(),
            asset_filter: attrs.get::<AssetFilter>().map(|a| a.0),
            units: attrs.get::<Units>().map(|u| u.0),
            edit_condition: attrs.get::<EditCondition>().map(|c| c.0),
            edit_condition_hides: attrs.contains::<EditConditionHides>(),
            toggle: attrs.contains::<Toggle>(),
            flags: attrs.get::<Flags>().map(|f| f.0),
            layer_mask: attrs.get::<LayerMask>().map(|l| l.0),
            file_picker: attrs.get::<FilePicker>().map(|f| f.0),
            dir_picker: attrs.contains::<DirPicker>(),
            button: attrs.get::<Button>().map(|b| b.0),
        }
    }

    /// The named bits of [`Flags`]: `(bit, name)`, bit 0 first, unnamed bits and bits past 63
    /// left out. Empty without the attribute.
    #[must_use]
    pub fn flag_names(&self) -> Vec<(u32, &'static str)> {
        self.flags
            .map(|names| {
                names
                    .split(',')
                    .map(str::trim)
                    .enumerate()
                    .filter(|(bit, name)| !name.is_empty() && *bit < 64)
                    .map(|(bit, name)| (bit as u32, name))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// True when no Regia Engine attribute is set.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::*;
    use bevy_reflect::{TypeInfo, Typed};

    #[derive(Reflect, Default)]
    #[reflect(Default)]
    #[reflect(@Category("Gameplay"))]
    struct Lamp {
        #[reflect(@Range(0.0..=1.0), @Step(0.05), @Tooltip("Brightness"), @Units("lm"))]
        intensity: f32,
        #[reflect(@DisplayName("Tint"), @ColorHdr)]
        color: [f32; 4],
        #[reflect(@Hidden, @ReadOnly)]
        internal: u32,
        #[reflect(@Multiline)]
        notes: String,
        #[reflect(@AssetFilter("*.ogg"))]
        hum: String,
        plain: bool,
    }

    #[test]
    fn attributes_read_back_from_type_info() {
        let TypeInfo::Struct(info) = Lamp::type_info() else {
            panic!("Lamp is a struct");
        };
        let attrs = |name: &str| {
            info.field(name)
                .map(|f| EditorAttrs::from_custom(f.custom_attributes()))
                .unwrap_or_default()
        };
        let intensity = attrs("intensity");
        assert_eq!(intensity.range, Some(0.0..=1.0));
        assert_eq!(intensity.step, Some(0.05));
        assert_eq!(intensity.tooltip, Some("Brightness"));
        assert_eq!(intensity.units, Some("lm"));
        let color = attrs("color");
        assert_eq!(color.display_name, Some("Tint"));
        assert!(color.color_hdr);
        let internal = attrs("internal");
        assert!(internal.hidden && internal.read_only);
        assert!(attrs("notes").multiline);
        assert_eq!(attrs("hum").asset_filter, Some("*.ogg"));
        assert!(attrs("plain").is_empty());
        assert_eq!(
            EditorAttrs::from_custom(info.custom_attributes()).category,
            Some("Gameplay")
        );
    }

    #[derive(Reflect, Default)]
    #[reflect(Default)]
    struct Kiln {
        #[reflect(@Flags("lit, , vented"))]
        state: u8,
        #[reflect(@LayerMask("physics"))]
        hits: u32,
        #[reflect(@FilePicker("*.png;*.jpg"))]
        decal: String,
        #[reflect(@DirPicker)]
        cache_dir: std::path::PathBuf,
        #[reflect(@Button("Bake"))]
        bake: (),
    }

    #[test]
    fn value_attributes_read_back() {
        let TypeInfo::Struct(info) = Kiln::type_info() else {
            panic!("Kiln is a struct");
        };
        let attrs = |name: &str| {
            info.field(name)
                .map(|f| EditorAttrs::from_custom(f.custom_attributes()))
                .unwrap_or_default()
        };
        assert_eq!(attrs("state").flag_names(), vec![(0, "lit"), (2, "vented")]);
        assert_eq!(attrs("hits").layer_mask, Some("physics"));
        assert_eq!(attrs("decal").file_picker, Some("*.png;*.jpg"));
        assert!(attrs("cache_dir").dir_picker);
        assert_eq!(attrs("bake").button, Some("Bake"));
        assert!(attrs("hits").flag_names().is_empty());
    }

    #[derive(Reflect, Default)]
    #[reflect(Default)]
    struct Malformed {
        #[reflect(@Range(180.0..=0.0), @Step(0.0))]
        reversed: f32,
        #[reflect(@Range(0.0..=f64::INFINITY), @Step(-0.1))]
        unbounded: f32,
        #[reflect(@Range(f64::NAN..=1.0), @Step(f64::NAN))]
        nan: f32,
    }

    #[test]
    fn malformed_range_and_step_are_dropped() {
        let TypeInfo::Struct(info) = Malformed::type_info() else {
            panic!("Malformed is a struct");
        };
        for name in ["reversed", "unbounded", "nan"] {
            let a = info
                .field(name)
                .map(|f| EditorAttrs::from_custom(f.custom_attributes()))
                .unwrap_or_default();
            assert_eq!(a.range, None, "{name} range");
            assert_eq!(a.step, None, "{name} step");
        }
    }

    #[derive(Reflect, Default)]
    #[reflect(Default)]
    struct Conditional {
        enabled: bool,
        #[reflect(@EditCondition("enabled"))]
        greyed: f32,
        #[reflect(@EditCondition("enabled && greyed > 1"), @EditConditionHides)]
        hidden: f32,
        #[reflect(@Toggle)]
        group: Lamp,
    }

    #[test]
    fn condition_and_toggle_attributes_read_back() {
        let TypeInfo::Struct(info) = Conditional::type_info() else {
            panic!("Conditional is a struct");
        };
        let attrs = |name: &str| {
            info.field(name)
                .map(|f| EditorAttrs::from_custom(f.custom_attributes()))
                .unwrap_or_default()
        };
        let greyed = attrs("greyed");
        assert_eq!(greyed.edit_condition, Some("enabled"));
        assert!(!greyed.edit_condition_hides);
        let hidden = attrs("hidden");
        assert_eq!(hidden.edit_condition, Some("enabled && greyed > 1"));
        assert!(hidden.edit_condition_hides);
        assert!(attrs("group").toggle);
        assert!(!attrs("enabled").toggle);
        assert!(attrs("enabled").is_empty());
    }
}
