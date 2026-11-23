// SPDX-License-Identifier: MPL-2.0
//! The curve and gradient editors' data side: key edits on the `FloatKey` curves and `ColorKey`
//! gradients of one emitter.
//!
//! `.vfx.ron` curves interpolate linearly between keys and carry no tangents, so a key is a
//! `(t, value)` pair and nothing else. Keys are kept sorted by `t` (a move that passes a neighbour
//! re-sorts, stably); `t` stays in `[0, 1]` (normalised particle life, or head-to-tail along a
//! ribbon).

use regia_vfx::format::{ColorKey, EmitterDef, FloatKey};

/// Most keys one curve may hold in the editor.
pub const MAX_KEYS: usize = 64;

/// A scalar curve of an emitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurveRef {
    /// `update.size_over_life` (multiplier of the spawn size).
    SizeOverLife,
    /// `update.alpha_over_life` (multiplier of the spawn alpha).
    AlphaOverLife,
    /// `update.rotation_over_life` (additive radians).
    RotationOverLife,
    /// `render.ribbon.width` (multiplier of the strip width, head to tail).
    RibbonWidth,
}

/// A colour gradient of an emitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GradientRef {
    /// `init.color` (sampled at spawn).
    InitColor,
    /// `update.color_over_life`.
    ColorOverLife,
    /// `render.ribbon.color` (head to tail).
    RibbonColor,
}

/// Every scalar curve.
pub const CURVES: [CurveRef; 4] = [
    CurveRef::SizeOverLife,
    CurveRef::AlphaOverLife,
    CurveRef::RotationOverLife,
    CurveRef::RibbonWidth,
];

/// Every gradient.
pub const GRADIENTS: [GradientRef; 3] = [
    GradientRef::InitColor,
    GradientRef::ColorOverLife,
    GradientRef::RibbonColor,
];

impl CurveRef {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SizeOverLife => "size_over_life",
            Self::AlphaOverLife => "alpha_over_life",
            Self::RotationOverLife => "rotation_over_life",
            Self::RibbonWidth => "ribbon_width",
        }
    }

    /// The curve called `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        CURVES.into_iter().find(|c| c.name() == name)
    }

    /// The value a curve with no keys stands for (what the simulation samples).
    #[must_use]
    pub fn empty_value(self) -> f32 {
        match self {
            Self::RotationOverLife => 0.0,
            _ => 1.0,
        }
    }

    /// The keys.
    #[must_use]
    pub fn keys(self, e: &EmitterDef) -> &[FloatKey] {
        match self {
            Self::SizeOverLife => &e.update.size_over_life,
            Self::AlphaOverLife => &e.update.alpha_over_life,
            Self::RotationOverLife => &e.update.rotation_over_life,
            Self::RibbonWidth => &e.render.ribbon.width,
        }
    }

    fn keys_mut(self, e: &mut EmitterDef) -> &mut Vec<FloatKey> {
        match self {
            Self::SizeOverLife => &mut e.update.size_over_life,
            Self::AlphaOverLife => &mut e.update.alpha_over_life,
            Self::RotationOverLife => &mut e.update.rotation_over_life,
            Self::RibbonWidth => &mut e.render.ribbon.width,
        }
    }
}

impl GradientRef {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::InitColor => "init_color",
            Self::ColorOverLife => "color_over_life",
            Self::RibbonColor => "ribbon_color",
        }
    }

    /// The gradient called `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        GRADIENTS.into_iter().find(|g| g.name() == name)
    }

    /// The keys.
    #[must_use]
    pub fn keys(self, e: &EmitterDef) -> &[ColorKey] {
        match self {
            Self::InitColor => &e.init.color,
            Self::ColorOverLife => &e.update.color_over_life,
            Self::RibbonColor => &e.render.ribbon.color,
        }
    }

    fn keys_mut(self, e: &mut EmitterDef) -> &mut Vec<ColorKey> {
        match self {
            Self::InitColor => &mut e.init.color,
            Self::ColorOverLife => &mut e.update.color_over_life,
            Self::RibbonColor => &mut e.render.ribbon.color,
        }
    }
}

/// One key edit.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyEdit {
    /// Adds a scalar key.
    AddKey(CurveRef, f32, f32),
    /// Moves scalar key `index` to `(t, v)`.
    MoveKey(CurveRef, usize, f32, f32),
    /// Deletes scalar key `index`.
    DeleteKey(CurveRef, usize),
    /// Adds a colour key.
    AddColor(GradientRef, f32, String),
    /// Moves colour key `index` to `t` with `color`.
    MoveColor(GradientRef, usize, f32, String),
    /// Deletes colour key `index`.
    DeleteColor(GradientRef, usize),
}

/// Why a key edit was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyError {
    /// `t` outside `[0, 1]`, a non-finite value or a bad colour.
    BadValue(String),
    /// No key at that index.
    BadIndex(String),
    /// Past [`MAX_KEYS`].
    TooMany(String),
}

fn check_t(t: f32) -> Result<f32, KeyError> {
    if t.is_finite() && (0.0..=1.0).contains(&t) {
        Ok(t)
    } else {
        Err(KeyError::BadValue(format!("key `t` must be in 0..=1, got {t}")))
    }
}

fn check_v(v: f32) -> Result<f32, KeyError> {
    if v.is_finite() && v.abs() <= super::modules::BIG {
        Ok(v)
    } else {
        Err(KeyError::BadValue(format!(
            "key value must be finite and within +-{}, got {v}",
            super::modules::BIG
        )))
    }
}

fn check_color(c: &str) -> Result<String, KeyError> {
    regia_core::math::Color::from_hex(c)
        .map(|_| c.to_owned())
        .map_err(|e| KeyError::BadValue(format!("bad colour `{c}`: {e}")))
}

fn bad_index(i: usize, len: usize) -> KeyError {
    KeyError::BadIndex(format!("no key {i} (the curve has {len})"))
}

/// Applies `edit` to `e`. Returns the index the edited key ends at (after sorting), or `None`
/// for a delete.
///
/// # Errors
///
/// See [`KeyError`].
pub fn apply_key_edit(e: &mut EmitterDef, edit: &KeyEdit) -> Result<Option<usize>, KeyError> {
    match edit {
        KeyEdit::AddKey(c, t, v) => {
            let key = FloatKey {
                t: check_t(*t)?,
                v: check_v(*v)?,
            };
            let keys = c.keys_mut(e);
            if keys.len() >= MAX_KEYS {
                return Err(KeyError::TooMany(format!(
                    "a curve holds at most {MAX_KEYS} keys"
                )));
            }
            // After every key with t <= the new t: equal times keep insertion order.
            let at = keys.partition_point(|k| k.t <= key.t);
            keys.insert(at, key);
            Ok(Some(at))
        }
        KeyEdit::MoveKey(c, i, t, v) => {
            let key = FloatKey {
                t: check_t(*t)?,
                v: check_v(*v)?,
            };
            let keys = c.keys_mut(e);
            let len = keys.len();
            if *i >= len {
                return Err(bad_index(*i, len));
            }
            keys.remove(*i);
            let at = keys.partition_point(|k| k.t <= key.t);
            keys.insert(at, key);
            Ok(Some(at))
        }
        KeyEdit::DeleteKey(c, i) => {
            let keys = c.keys_mut(e);
            if *i >= keys.len() {
                return Err(bad_index(*i, keys.len()));
            }
            keys.remove(*i);
            Ok(None)
        }
        KeyEdit::AddColor(g, t, color) => {
            let key = ColorKey {
                t: check_t(*t)?,
                color: check_color(color)?,
            };
            let keys = g.keys_mut(e);
            if keys.len() >= MAX_KEYS {
                return Err(KeyError::TooMany(format!(
                    "a gradient holds at most {MAX_KEYS} keys"
                )));
            }
            let at = keys.partition_point(|k| k.t <= key.t);
            keys.insert(at, key);
            Ok(Some(at))
        }
        KeyEdit::MoveColor(g, i, t, color) => {
            let key = ColorKey {
                t: check_t(*t)?,
                color: check_color(color)?,
            };
            let keys = g.keys_mut(e);
            let len = keys.len();
            if *i >= len {
                return Err(bad_index(*i, len));
            }
            keys.remove(*i);
            let at = keys.partition_point(|k| k.t <= key.t);
            keys.insert(at, key);
            Ok(Some(at))
        }
        KeyEdit::DeleteColor(g, i) => {
            let keys = g.keys_mut(e);
            if *i >= keys.len() {
                return Err(bad_index(*i, keys.len()));
            }
            keys.remove(*i);
            Ok(None)
        }
    }
}
