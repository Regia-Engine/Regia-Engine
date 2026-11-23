// SPDX-License-Identifier: MPL-2.0
//! The recipe's `view_range` and `camera_default` blocks (`jewelry.piece_recipe/1`), parsed and
//! validated. Every value the controller clamps to comes from here.

use glam::Vec3;
use serde_json::Value;

/// The largest tilt cone a recipe may ask for. A cone of 90 degrees or more would let the camera
/// reach the side of the piece, which is what `spin` modes are for.
pub const TILT_LIMIT_DEG: f32 = 89.0;

/// The largest pan offset, in mm, a recipe may ask for on either axis.
pub const PAN_LIMIT_MM: f32 = 1000.0;

/// The pitch limit of a free orbit, in degrees (keeps the camera off the poles).
pub const FREE_PITCH_DEG: f32 = 85.0;

/// What `spin` the recipe allows (`view_range.spin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpinMode {
    /// The default: tilt inside the cone only; no turn about the front axis.
    None,
    /// A wider side-to-side rock: horizontal tilt up to `rock_max_deg`, vertical tilt up to
    /// `tilt_max_deg` (an elliptical cone).
    Rock,
    /// Tilt inside the cone, plus a free turn of the piece about its front axis (two-finger twist).
    AboutFrontAxis,
    /// A full orbit: the horizontal angle wraps, the vertical angle stops at `FREE_PITCH_DEG`.
    Free,
}

impl SpinMode {
    /// The wire name (`none`, `rock`, `about_front_axis`, `free`).
    pub fn as_str(self) -> &'static str {
        match self {
            SpinMode::None => "none",
            SpinMode::Rock => "rock",
            SpinMode::AboutFrontAxis => "about_front_axis",
            SpinMode::Free => "free",
        }
    }
}

/// A refused `view_range` or `camera_default`: a stable error code plus a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewRangeError {
    /// Lowercase ascii code, part of the agent protocol (`book/agents/viewer-view.md`).
    pub code: &'static str,
    /// What was wrong, naming the field.
    pub message: String,
}

impl std::fmt::Display for ViewRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ViewRangeError {}

fn err(code: &'static str, message: impl Into<String>) -> ViewRangeError {
    ViewRangeError {
        code,
        message: message.into(),
    }
}

/// The limits of the drag view, from a recipe's `view_range`.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewRange {
    /// Unit vector out of the piece's front, in the piece's frame. The default view looks down it.
    pub front_axis: Vec3,
    /// Half-angle of the tilt cone around `front_axis`, in degrees.
    pub tilt_max_deg: f32,
    /// Pan bounds in mm in the front plane: `[[x_min, x_max], [y_min, y_max]]` (x = right, y = up).
    pub pan_bounds_mm: [[f32; 2]; 2],
    /// What turn the recipe allows.
    pub spin: SpinMode,
    /// With `SpinMode::Rock`: the horizontal limit in degrees (at least `tilt_max_deg`).
    pub rock_max_deg: Option<f32>,
    /// The closest zoom: at most this many screen pixels per mm of the piece.
    pub max_px_per_mm: f32,
    /// How faithful the back is (`exact`, `plausible`, `generic`); informational for the viewer.
    pub back: String,
}

impl Default for ViewRange {
    /// The operator's defaults (2026-11-04, decision D4): front view, a 30 degree cone, no spin.
    fn default() -> Self {
        ViewRange {
            front_axis: Vec3::Z,
            tilt_max_deg: 30.0,
            pan_bounds_mm: [[-3.0, 3.0], [-3.0, 3.0]],
            spin: SpinMode::None,
            rock_max_deg: None,
            max_px_per_mm: 400.0,
            back: "plausible".to_string(),
        }
    }
}

fn num(v: &Value, field: &str) -> Result<f32, ViewRangeError> {
    let f = v
        .as_f64()
        .ok_or_else(|| err("view_range_bad_value", format!("{field} is not a number")))?;
    let f = f as f32;
    if !f.is_finite() {
        return Err(err("view_range_bad_value", format!("{field} is not finite")));
    }
    Ok(f)
}

fn vec3(v: &Value, field: &str) -> Result<Vec3, ViewRangeError> {
    let a = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| err("view_range_bad_value", format!("{field} is not [x, y, z]")))?;
    Ok(Vec3::new(
        num(&a[0], field)?,
        num(&a[1], field)?,
        num(&a[2], field)?,
    ))
}

impl ViewRange {
    /// Parse a recipe's `view_range` object. Missing fields take the defaults; a present field that
    /// is malformed or out of range is refused (never silently widened).
    pub fn from_json(v: &Value) -> Result<ViewRange, ViewRangeError> {
        let obj = v
            .as_object()
            .ok_or_else(|| err("view_range_bad_value", "view_range is not an object"))?;
        let mut r = ViewRange::default();
        if let Some(a) = obj.get("front_axis") {
            let f = vec3(a, "front_axis")?;
            if f.length() < 1e-6 {
                return Err(err("view_range_bad_axis", "front_axis has zero length"));
            }
            r.front_axis = f.normalize();
        }
        if let Some(t) = obj.get("tilt_max_deg") {
            r.tilt_max_deg = num(t, "tilt_max_deg")?;
        }
        if let Some(p) = obj.get("pan_bounds_mm") {
            let rows = p.as_array().filter(|a| a.len() == 2).ok_or_else(|| {
                err(
                    "view_range_bad_value",
                    "pan_bounds_mm is not [[x0, x1], [y0, y1]]",
                )
            })?;
            for (i, row) in rows.iter().enumerate() {
                let pair = row
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .ok_or_else(|| err("view_range_bad_value", "pan_bounds_mm row is not [min, max]"))?;
                r.pan_bounds_mm[i] = [num(&pair[0], "pan_bounds_mm")?, num(&pair[1], "pan_bounds_mm")?];
            }
        }
        if let Some(s) = obj.get("spin") {
            r.spin = serde_json::from_value(s.clone()).map_err(|_| {
                err(
                    "view_range_bad_spin",
                    format!("spin {s} is not none, rock, about_front_axis or free"),
                )
            })?;
        }
        if let Some(rm) = obj.get("rock_max_deg").filter(|v| !v.is_null()) {
            r.rock_max_deg = Some(num(rm, "rock_max_deg")?);
        }
        if let Some(m) = obj.get("max_px_per_mm") {
            r.max_px_per_mm = num(m, "max_px_per_mm")?;
        }
        if let Some(b) = obj.get("back") {
            r.back = b
                .as_str()
                .ok_or_else(|| err("view_range_bad_value", "back is not a string"))?
                .to_string();
        }
        r.validate()?;
        Ok(r)
    }

    /// Check every limit is usable; the controller only accepts a validated range.
    pub fn validate(&self) -> Result<(), ViewRangeError> {
        if !self.front_axis.is_finite() || (self.front_axis.length() - 1.0).abs() > 1e-3 {
            return Err(err("view_range_bad_axis", "front_axis is not a unit vector"));
        }
        if !(0.0..=TILT_LIMIT_DEG).contains(&self.tilt_max_deg) {
            return Err(err(
                "view_range_bad_tilt",
                format!(
                    "tilt_max_deg {} is outside 0..={TILT_LIMIT_DEG}",
                    self.tilt_max_deg
                ),
            ));
        }
        for (axis, [lo, hi]) in ["x", "y"].iter().zip(self.pan_bounds_mm) {
            if !(lo.is_finite() && hi.is_finite()) || lo > hi || lo < -PAN_LIMIT_MM || hi > PAN_LIMIT_MM {
                return Err(err(
                    "view_range_bad_pan",
                    format!("pan_bounds_mm {axis} [{lo}, {hi}] is not min <= max within +-{PAN_LIMIT_MM}"),
                ));
            }
        }
        match (self.spin, self.rock_max_deg) {
            (SpinMode::Rock, None) => {
                return Err(err("view_range_bad_rock", "spin rock needs rock_max_deg"));
            }
            (SpinMode::Rock, Some(r)) if !(0.0..=180.0).contains(&r) => {
                return Err(err(
                    "view_range_bad_rock",
                    format!("rock_max_deg {r} is outside 0..=180"),
                ));
            }
            _ => {}
        }
        if !(self.max_px_per_mm.is_finite() && self.max_px_per_mm > 0.0) {
            return Err(err("view_range_bad_zoom", "max_px_per_mm must be positive"));
        }
        Ok(())
    }

    /// The horizontal tilt limit in degrees (`rock_max_deg` for a rock, `tilt_max_deg` otherwise;
    /// unbounded for a free orbit).
    pub fn horizontal_limit_deg(&self) -> f32 {
        match self.spin {
            SpinMode::Rock => self
                .rock_max_deg
                .unwrap_or(self.tilt_max_deg)
                .max(self.tilt_max_deg),
            SpinMode::Free => f32::INFINITY,
            _ => self.tilt_max_deg,
        }
    }

    /// The vertical tilt limit in degrees.
    pub fn vertical_limit_deg(&self) -> f32 {
        match self.spin {
            SpinMode::Free => FREE_PITCH_DEG,
            _ => self.tilt_max_deg,
        }
    }

    /// The largest angle, in degrees, between the view direction and `front_axis` this range allows.
    pub fn max_view_angle_deg(&self) -> f32 {
        match self.spin {
            SpinMode::Free => 180.0,
            SpinMode::Rock => self.horizontal_limit_deg(),
            _ => self.tilt_max_deg,
        }
    }
}

/// The recipe's `camera_default` block: the framing at zoom 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraDefaults {
    /// Camera distance from the pivot, in mm.
    pub distance_mm: f32,
    /// Lens focal length at zoom 1, in mm.
    pub focal_length_mm: f32,
    /// Sensor width in mm (36 = full frame).
    pub sensor_mm: f32,
    /// Aperture, carried into the build (the cheap viewer draws no depth of field).
    pub f_stop: f32,
}

impl Default for CameraDefaults {
    fn default() -> Self {
        CameraDefaults {
            distance_mm: 60.0,
            focal_length_mm: 100.0,
            sensor_mm: 36.0,
            f_stop: 11.0,
        }
    }
}

impl CameraDefaults {
    /// Parse a recipe's `camera_default` object; missing fields take the defaults.
    pub fn from_json(v: &Value) -> Result<CameraDefaults, ViewRangeError> {
        let obj = v
            .as_object()
            .ok_or_else(|| err("view_range_bad_value", "camera_default is not an object"))?;
        let mut c = CameraDefaults::default();
        for (field, slot) in [
            ("distance_mm", &mut c.distance_mm),
            ("focal_length_mm", &mut c.focal_length_mm),
            ("sensor_mm", &mut c.sensor_mm),
            ("f_stop", &mut c.f_stop),
        ] {
            if let Some(x) = obj.get(field) {
                let x = num(x, field)?;
                if !(x > 0.0 && x < 1.0e6) {
                    return Err(err(
                        "view_range_bad_camera",
                        format!("{field} {x} must be positive"),
                    ));
                }
                *slot = x;
            }
        }
        Ok(c)
    }

    /// Field of view of the lens at zoom 1, in radians: across the sensor, mapped to the viewport's shorter side.
    pub fn lens_fov_rad(&self) -> f32 {
        2.0 * (self.sensor_mm / (2.0 * self.focal_length_mm)).atan()
    }
}

/// Parse both blocks from a whole recipe document (`view_range` and `camera_default`, each optional).
pub fn from_recipe(recipe: &Value) -> Result<(ViewRange, CameraDefaults), ViewRangeError> {
    let range = match recipe.get("view_range") {
        Some(v) => ViewRange::from_json(v)?,
        None => ViewRange::default(),
    };
    let cam = match recipe.get("camera_default") {
        Some(v) => CameraDefaults::from_json(v)?,
        None => CameraDefaults::default(),
    };
    Ok((range, cam))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interfaces_example_parses() {
        let r = ViewRange::from_json(&json!({"front_axis": [0, 0, 1], "tilt_max_deg": 30,
            "pan_bounds_mm": [[-3, 3], [-3, 3]], "spin": "none", "rock_max_deg": null,
            "max_px_per_mm": 400, "back": "plausible"}))
        .expect("ok");
        assert_eq!(r, ViewRange::default());
    }

    #[test]
    fn bad_values_are_refused_with_codes() {
        let cases = [
            (json!({"tilt_max_deg": 95}), "view_range_bad_tilt"),
            (json!({"tilt_max_deg": -1}), "view_range_bad_tilt"),
            (json!({"front_axis": [0, 0, 0]}), "view_range_bad_axis"),
            (json!({"pan_bounds_mm": [[3, -3], [0, 0]]}), "view_range_bad_pan"),
            (json!({"spin": "wobble"}), "view_range_bad_spin"),
            (json!({"spin": "rock"}), "view_range_bad_rock"),
            (json!({"max_px_per_mm": 0}), "view_range_bad_zoom"),
            (json!({"tilt_max_deg": "x"}), "view_range_bad_value"),
            (json!([1]), "view_range_bad_value"),
        ];
        for (v, code) in cases {
            assert_eq!(ViewRange::from_json(&v).expect_err("refused").code, code, "{v}");
        }
    }

    #[test]
    fn rock_widens_only_the_horizontal_limit() {
        let r = ViewRange::from_json(&json!({"spin": "rock", "rock_max_deg": 45})).expect("ok");
        assert_eq!(r.horizontal_limit_deg(), 45.0);
        assert_eq!(r.vertical_limit_deg(), 30.0);
        let narrow = ViewRange::from_json(&json!({"spin": "rock", "rock_max_deg": 10})).expect("ok");
        assert_eq!(narrow.horizontal_limit_deg(), 30.0);
    }

    #[test]
    fn camera_defaults_parse() {
        let c = CameraDefaults::from_json(&json!({"distance_mm": 80, "focal_length_mm": 50})).expect("ok");
        assert_eq!(c.distance_mm, 80.0);
        assert_eq!(c.sensor_mm, 36.0);
        assert!(CameraDefaults::from_json(&json!({"distance_mm": -1})).is_err());
    }
}
