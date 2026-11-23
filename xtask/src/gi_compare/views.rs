// SPDX-License-Identifier: MPL-2.0
//! `views.ron`: the cameras `gi-compare` renders, the output size and exposures, and per view an
//! optional interior box for the leak metric.
//!
//! ```ron
//! (
//!     format_version: 1,
//!     size: (320, 240),
//!     ev100: 10.0,
//!     indirect_ev100: Some(8.0),
//!     views: [
//!         (
//!             name: "front",
//!             eye: (0.0, 1.0, 3.4),
//!             target: (0.0, 1.0, 0.0),
//!             fov_y_deg: 45.0,
//!             interior: Some((min: (-1.0, 0.0, -1.0), max: (1.0, 2.0, 1.0))),
//!             leak_threshold: Some(0.25),
//!         ),
//!     ],
//! )
//! ```
//!
//! An optional `flythrough` list of camera keys (at least two) asks for a strip of baked frames
//! along the path through them (`--flythrough-frames`, ADR-1326):
//!
//! ```ron
//! flythrough: [
//!     (eye: (6.0, 1.7, 9.0), target: (0.0, 1.2, 2.0), fov_y_deg: 60.0),
//!     (eye: (0.0, 1.6, 1.0), target: (0.0, 1.2, -3.0), fov_y_deg: 70.0, ev100: Some(11.0)),
//! ],
//! ```
//!
//! Parsing never panics; every rejection is a [`ViewsError`] with a stable lowercase `code`.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// The only `views.ron` version this build reads.
pub const VIEWS_FORMAT_VERSION: u32 = 1;

/// Largest output edge, pixels (a reference trace at this size and high spp already takes long).
pub const MAX_EDGE: u32 = 4096;

/// Default leak threshold: the bake may put this fraction more indirect light inside an interior
/// box than the reference before the verdict flags it.
pub const DEFAULT_LEAK_THRESHOLD: f32 = 0.25;

/// An axis-aligned world box.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldBox {
    /// Lower corner, metres.
    pub min: [f32; 3],
    /// Upper corner, metres.
    pub max: [f32; 3],
}

impl WorldBox {
    /// Whether `p` lies in the box grown by `margin` on every side.
    #[must_use]
    pub fn contains(&self, p: [f32; 3], margin: f32) -> bool {
        (0..3).all(|a| p[a] >= self.min[a] - margin && p[a] <= self.max[a] + margin)
    }

    fn is_valid(&self) -> bool {
        (0..3).all(|a| self.min[a].is_finite() && self.max[a].is_finite() && self.min[a] < self.max[a])
    }
}

/// One named camera.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    /// File-name-safe name (`[a-z0-9_-]`), unique in the file.
    pub name: String,
    /// Camera position, metres.
    pub eye: [f32; 3],
    /// Look-at point.
    pub target: [f32; 3],
    /// Vertical field of view, degrees (the raster frame runs without Hor+, so this is exact).
    pub fov_y_deg: f32,
    /// The room's air volume for the leak metric: pixels whose reference first hit lies in it
    /// (grown by 5 cm, so its walls count) form the interior mask.
    #[serde(default)]
    pub interior: Option<WorldBox>,
    /// Leak threshold for this view ([`DEFAULT_LEAK_THRESHOLD`] when absent).
    #[serde(default)]
    pub leak_threshold: Option<f32>,
    /// Exposure of this view's full tiles (an interior view next to a sunlit one); the set's
    /// when absent. Every tile of one row still shares one exposure.
    #[serde(default)]
    pub ev100: Option<f32>,
    /// Exposure of this view's indirect tiles; the set's when absent.
    #[serde(default)]
    pub indirect_ev100: Option<f32>,
}

impl View {
    /// The leak threshold in effect.
    #[must_use]
    pub fn leak_threshold(&self) -> f32 {
        self.leak_threshold.unwrap_or(DEFAULT_LEAK_THRESHOLD)
    }
}

/// One key of the fly-through path: a camera, and optionally its exposure.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlyKey {
    /// Camera position, metres.
    pub eye: [f32; 3],
    /// Look-at point.
    pub target: [f32; 3],
    /// Vertical field of view, degrees.
    pub fov_y_deg: f32,
    /// Exposure at this key (EV100); the set's `ev100` when absent. Frames between two keys
    /// interpolate it, so a path from a sunlit yard into a room can open up on the way.
    #[serde(default)]
    pub ev100: Option<f32>,
}

/// A parsed, validated `views.ron`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSet {
    /// [`VIEWS_FORMAT_VERSION`].
    pub format_version: u32,
    /// Output width and height, pixels, for every view.
    pub size: (u32, u32),
    /// Exposure of the full-image tiles (EV100), the same on every tile.
    pub ev100: f32,
    /// Exposure of the indirect-only tiles; `ev100 − 2` when absent (indirect light is dimmer).
    #[serde(default)]
    pub indirect_ev100: Option<f32>,
    /// The cameras, rendered in this order.
    pub views: Vec<View>,
    /// Keys of the fly-through path (none, or at least two): `--flythrough-frames` baked frames
    /// are spaced evenly along the eye's path through them (ADR-1326).
    #[serde(default)]
    pub flythrough: Vec<FlyKey>,
}

impl ViewSet {
    /// The set's indirect-tile exposure.
    #[must_use]
    pub fn indirect_ev100(&self) -> f32 {
        self.indirect_ev100.unwrap_or(self.ev100 - 2.0)
    }

    /// The full and indirect exposures of `view`: its own, else the set's. A view that sets only
    /// `ev100` gets `ev100 − 2` for the indirect row.
    #[must_use]
    pub fn exposures(&self, view: &View) -> (f32, f32) {
        match (view.ev100, view.indirect_ev100) {
            (Some(ev), Some(iev)) => (ev, iev),
            (Some(ev), None) => (ev, ev - 2.0),
            (None, Some(iev)) => (self.ev100, iev),
            (None, None) => (self.ev100, self.indirect_ev100()),
        }
    }

    /// `frames` cameras spaced evenly by eye distance along the fly-through path, named
    /// `fly00`, `fly01`, …, each with its exposure as `ev100`. Eye, target, field of view and
    /// exposure are interpolated linearly between the two keys around each frame. A path whose
    /// eye does not move (a pan) is spaced evenly by key instead. Empty when the set has no path
    /// or `frames` is 0; one frame is the first key.
    #[must_use]
    pub fn flythrough_views(&self, frames: u32) -> Vec<View> {
        let keys = &self.flythrough;
        if keys.len() < 2 || frames == 0 {
            return Vec::new();
        }
        let lengths: Vec<f32> = keys
            .windows(2)
            .map(|w| {
                let d = [0, 1, 2].map(|a| w[1].eye[a] - w[0].eye[a]);
                (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
            })
            .collect();
        let total: f32 = lengths.iter().sum();
        let lengths = if total > 1e-6 {
            lengths
        } else {
            vec![1.0; lengths.len()]
        };
        let total: f32 = lengths.iter().sum();
        let lerp3 = |a: [f32; 3], b: [f32; 3], t: f32| [0, 1, 2].map(|k| a[k] + (b[k] - a[k]) * t);
        (0..frames)
            .map(|i| {
                let mut s = if frames > 1 {
                    total * i as f32 / (frames - 1) as f32
                } else {
                    0.0
                };
                let mut seg = 0;
                while seg + 1 < lengths.len() && s > lengths[seg] {
                    s -= lengths[seg];
                    seg += 1;
                }
                let t = if lengths[seg] > 0.0 {
                    (s / lengths[seg]).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (a, b) = (&keys[seg], &keys[seg + 1]);
                let ev_a = a.ev100.unwrap_or(self.ev100);
                let ev_b = b.ev100.unwrap_or(self.ev100);
                View {
                    name: format!("fly{i:02}"),
                    eye: lerp3(a.eye, b.eye, t),
                    target: lerp3(a.target, b.target, t),
                    fov_y_deg: a.fov_y_deg + (b.fov_y_deg - a.fov_y_deg) * t,
                    interior: None,
                    leak_threshold: None,
                    ev100: Some(ev_a + (ev_b - ev_a) * t),
                    indirect_ev100: None,
                }
            })
            .collect()
    }
}

/// Why a `views.ron` was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewsError {
    /// Not RON, or not the expected shape (unknown field, wrong type).
    Parse(String),
    /// `format_version` is not [`VIEWS_FORMAT_VERSION`].
    Version(u32),
    /// The size is zero or larger than [`MAX_EDGE`].
    Size(u32, u32),
    /// An exposure is not finite.
    Exposure,
    /// The file lists no view.
    NoViews,
    /// A name is empty or uses characters outside `[a-z0-9_-]`.
    BadName(String),
    /// Two views share a name.
    DuplicateName(String),
    /// Eye or target is not finite, or they coincide, or the camera looks straight up or down
    /// (no right vector against +Y).
    Camera(String),
    /// The field of view is outside (1°, 170°).
    Fov(String),
    /// The interior box is empty, inverted or not finite.
    Interior(String),
    /// The leak threshold is not a finite number ≥ 0.
    LeakThreshold(String),
    /// The fly-through path has one key, or a key (or a frame between two keys) has a bad camera,
    /// field of view or exposure.
    Flythrough(String),
}

impl ViewsError {
    /// Stable error code (lowercase ascii, digits and `_`), documented in `book/agents/gi-compare.md`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "views_parse",
            Self::Version(_) => "views_version",
            Self::Size(..) => "views_size",
            Self::Exposure => "views_exposure",
            Self::NoViews => "views_empty",
            Self::BadName(_) => "view_name",
            Self::DuplicateName(_) => "view_duplicate",
            Self::Camera(_) => "view_camera",
            Self::Fov(_) => "view_fov",
            Self::Interior(_) => "view_interior",
            Self::LeakThreshold(_) => "view_leak_threshold",
            Self::Flythrough(_) => "views_flythrough",
        }
    }
}

impl std::fmt::Display for ViewsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(m) => write!(f, "views.ron does not parse: {m}"),
            Self::Version(v) => write!(
                f,
                "views.ron format_version {v} is not supported (this build reads {VIEWS_FORMAT_VERSION})"
            ),
            Self::Size(w, h) => write!(f, "size ({w}, {h}) must be 1..={MAX_EDGE} on each side"),
            Self::Exposure => write!(f, "every ev100 and indirect_ev100 must be a finite number"),
            Self::NoViews => write!(f, "views.ron lists no view"),
            Self::BadName(n) => write!(
                f,
                "view name {n:?} must be non-empty and use only a-z, 0-9, _ and -"
            ),
            Self::DuplicateName(n) => write!(f, "two views are named {n:?}"),
            Self::Camera(n) => write!(
                f,
                "view {n:?}: eye and target must be finite, distinct, and not straight above or below each other"
            ),
            Self::Fov(n) => write!(f, "view {n:?}: fov_y_deg must be between 1 and 170"),
            Self::Interior(n) => write!(f, "view {n:?}: interior min must be below max on every axis"),
            Self::LeakThreshold(n) => write!(f, "view {n:?}: leak_threshold must be a finite number >= 0"),
            Self::Flythrough(m) => write!(f, "flythrough: {m}"),
        }
    }
}

impl std::error::Error for ViewsError {}

fn finite3(v: [f32; 3]) -> bool {
    v.iter().all(|x| x.is_finite())
}

/// Eye and target are finite and distinct, and the camera does not look straight up or down (it
/// needs a right vector against +Y).
fn camera_ok(eye: [f32; 3], target: [f32; 3]) -> bool {
    let d = [0, 1, 2].map(|a| target[a] - eye[a]);
    let horizontal = (d[0] * d[0] + d[2] * d[2]).sqrt();
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    finite3(eye) && finite3(target) && len >= 1e-4 && horizontal >= 1e-3 * len
}

fn fov_ok(fov_y_deg: f32) -> bool {
    fov_y_deg.is_finite() && fov_y_deg > 1.0 && fov_y_deg < 170.0
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Parses and validates `views.ron` text.
///
/// # Errors
///
/// The first problem found, as a [`ViewsError`].
pub fn parse_views(text: &str) -> Result<ViewSet, ViewsError> {
    let set: ViewSet = ron::from_str(text).map_err(|e| ViewsError::Parse(e.to_string()))?;
    validate(&set)?;
    Ok(set)
}

/// Checks a [`ViewSet`] built in code or parsed.
///
/// # Errors
///
/// The first problem found.
pub fn validate(set: &ViewSet) -> Result<(), ViewsError> {
    if set.format_version != VIEWS_FORMAT_VERSION {
        return Err(ViewsError::Version(set.format_version));
    }
    let (w, h) = set.size;
    if w == 0 || h == 0 || w > MAX_EDGE || h > MAX_EDGE {
        return Err(ViewsError::Size(w, h));
    }
    let finite_exposures = set.ev100.is_finite()
        && set.indirect_ev100().is_finite()
        && set.views.iter().all(|v| {
            let (ev, iev) = set.exposures(v);
            ev.is_finite() && iev.is_finite()
        });
    if !finite_exposures {
        return Err(ViewsError::Exposure);
    }
    if set.views.is_empty() {
        return Err(ViewsError::NoViews);
    }
    let mut seen = HashSet::new();
    for view in &set.views {
        if !valid_name(&view.name) {
            return Err(ViewsError::BadName(view.name.clone()));
        }
        if !seen.insert(view.name.as_str()) {
            return Err(ViewsError::DuplicateName(view.name.clone()));
        }
        if !camera_ok(view.eye, view.target) {
            return Err(ViewsError::Camera(view.name.clone()));
        }
        if !fov_ok(view.fov_y_deg) {
            return Err(ViewsError::Fov(view.name.clone()));
        }
        if let Some(b) = &view.interior
            && !b.is_valid()
        {
            return Err(ViewsError::Interior(view.name.clone()));
        }
        if let Some(t) = view.leak_threshold
            && !(t.is_finite() && t >= 0.0)
        {
            return Err(ViewsError::LeakThreshold(view.name.clone()));
        }
    }
    validate_flythrough(set)
}

fn validate_flythrough(set: &ViewSet) -> Result<(), ViewsError> {
    if set.flythrough.is_empty() {
        return Ok(());
    }
    if set.flythrough.len() < 2 {
        return Err(ViewsError::Flythrough("a path needs at least two keys".into()));
    }
    for (i, key) in set.flythrough.iter().enumerate() {
        if !camera_ok(key.eye, key.target) || !fov_ok(key.fov_y_deg) {
            return Err(ViewsError::Flythrough(format!(
                "key {i}: eye and target must be finite, distinct and not straight above or below each \
                 other, and fov_y_deg between 1 and 170"
            )));
        }
        if key.ev100.is_some_and(|ev| !ev.is_finite()) {
            return Err(ViewsError::Flythrough(format!("key {i}: ev100 must be finite")));
        }
    }
    // Between two keys the view vector `target − eye` moves linearly, so its horizontal part is
    // closest to zero at one parameter per segment: check the camera there.
    for (i, w) in set.flythrough.windows(2).enumerate() {
        let d0 = [0, 1, 2].map(|k| w[0].target[k] - w[0].eye[k]);
        let d1 = [0, 1, 2].map(|k| w[1].target[k] - w[1].eye[k]);
        let (ax, az) = (d0[0], d0[2]);
        let (bx, bz) = (d1[0] - d0[0], d1[2] - d0[2]);
        let len2 = bx * bx + bz * bz;
        let t = if len2 > 0.0 {
            (-(ax * bx + az * bz) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let at = |a: [f32; 3], b: [f32; 3]| [0, 1, 2].map(|k| a[k] + (b[k] - a[k]) * t);
        if !camera_ok(at(w[0].eye, w[1].eye), at(w[0].target, w[1].target)) {
            return Err(ViewsError::Flythrough(format!(
                "between keys {i} and {} the camera looks straight up or down",
                i + 1
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"(
        format_version: 1,
        size: (64, 48),
        ev100: 10.0,
        views: [
            (name: "front", eye: (0.0, 1.0, 3.0), target: (0.0, 1.0, 0.0), fov_y_deg: 45.0),
            (
                name: "inside_2",
                eye: (0.0, 1.5, 0.5),
                target: (1.0, 1.0, -1.0),
                fov_y_deg: 70.0,
                interior: Some((min: (-1.0, 0.0, -1.0), max: (1.0, 2.0, 1.0))),
                leak_threshold: Some(0.1),
                ev100: Some(12.5),
            ),
        ],
    )"#;

    fn with_view(view: &str) -> String {
        format!("(format_version: 1, size: (64, 48), ev100: 10.0, views: [{view}])")
    }

    #[test]
    fn a_good_file_parses_with_defaults() {
        let set = parse_views(GOOD).expect("parses");
        assert_eq!(set.views.len(), 2);
        assert_eq!(set.indirect_ev100(), 8.0);
        assert_eq!(set.views[0].interior, None);
        assert_eq!(set.views[0].leak_threshold(), DEFAULT_LEAK_THRESHOLD);
        assert_eq!(set.views[1].leak_threshold(), 0.1);
        assert_eq!(set.exposures(&set.views[0]), (10.0, 8.0));
        assert_eq!(
            set.exposures(&set.views[1]),
            (12.5, 10.5),
            "the view's own, indirect 2 EV under"
        );
        let b = set.views[1].interior.expect("interior");
        assert!(b.contains([0.0, 1.0, 0.0], 0.0));
        assert!(b.contains([1.04, 1.0, 0.0], 0.05));
        assert!(!b.contains([1.2, 1.0, 0.0], 0.05));
    }

    #[test]
    fn each_bad_view_is_a_typed_error() {
        let cases = [
            (
                with_view(
                    "(name: \"Bad Name\", eye: (0.0, 1.0, 3.0), target: (0.0, 1.0, 0.0), fov_y_deg: 45.0)",
                ),
                "view_name",
            ),
            (
                with_view("(name: \"a\", eye: (0.0, 1.0, 3.0), target: (0.0, 1.0, 3.0), fov_y_deg: 45.0)"),
                "view_camera",
            ),
            (
                with_view("(name: \"a\", eye: (0.0, 5.0, 0.0), target: (0.0, 0.0, 0.0), fov_y_deg: 45.0)"),
                "view_camera",
            ),
            (
                with_view("(name: \"a\", eye: (0.0, 1.0, 3.0), target: (0.0, 1.0, 0.0), fov_y_deg: 179.0)"),
                "view_fov",
            ),
            (
                with_view(
                    "(name: \"a\", eye: (0.0, 1.0, 3.0), target: (0.0, 1.0, 0.0), fov_y_deg: 45.0, \
                     interior: Some((min: (1.0, 0.0, 0.0), max: (0.0, 1.0, 1.0))))",
                ),
                "view_interior",
            ),
            (
                with_view(
                    "(name: \"a\", eye: (0.0, 1.0, 3.0), target: (0.0, 1.0, 0.0), fov_y_deg: 45.0, \
                     leak_threshold: Some(-1.0))",
                ),
                "view_leak_threshold",
            ),
            (
                with_view(
                    "(name: \"a\", eye: (0.0, 1.0, 3.0), target: (0.0, 1.0, 0.0), fov_y_deg: 45.0), \
                     (name: \"a\", eye: (0.0, 1.0, 3.0), target: (0.0, 1.0, 0.0), fov_y_deg: 45.0)",
                ),
                "view_duplicate",
            ),
            (
                with_view("(name: \"a\", eye: (0.0, 1.0, 3.0), target: (0.0, 1.0, 0.0), fov: 45.0)"),
                "views_parse",
            ),
            (with_view(""), "views_empty"),
            (
                "(format_version: 2, size: (64, 48), ev100: 10.0, views: [])".to_string(),
                "views_version",
            ),
            (
                "(format_version: 1, size: (0, 48), ev100: 10.0, views: [])".to_string(),
                "views_size",
            ),
        ];
        for (text, code) in cases {
            let err = parse_views(&text).expect_err(code);
            assert_eq!(err.code(), code, "{text}: {err}");
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn error_codes_are_lowercase_ascii() {
        let all = [
            ViewsError::Parse(String::new()),
            ViewsError::Version(0),
            ViewsError::Size(0, 0),
            ViewsError::Exposure,
            ViewsError::NoViews,
            ViewsError::BadName(String::new()),
            ViewsError::DuplicateName(String::new()),
            ViewsError::Camera(String::new()),
            ViewsError::Fov(String::new()),
            ViewsError::Interior(String::new()),
            ViewsError::LeakThreshold(String::new()),
            ViewsError::Flythrough(String::new()),
        ];
        for e in all {
            assert!(
                e.code()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                e.code()
            );
        }
    }

    fn with_path(keys: &str) -> String {
        format!(
            "(format_version: 1, size: (64, 48), ev100: 10.0, views: [(name: \"a\", eye: (0.0, 1.0, 3.0), \
             target: (0.0, 1.0, 0.0), fov_y_deg: 45.0)], flythrough: [{keys}])"
        )
    }

    /// No path: no frames; the field is optional, so older views files still parse.
    #[test]
    fn a_file_without_a_path_has_no_flythrough() {
        let set = parse_views(GOOD).expect("parses");
        assert!(set.flythrough.is_empty());
        assert!(set.flythrough_views(6).is_empty());
    }

    /// Frames are spaced evenly by eye distance: on a path of 3 m then 1 m, five frames land at
    /// 0, 1, 2, 3 and 4 m, the fourth exactly on the middle key. Target, field of view and
    /// exposure interpolate with the eye; a key without `ev100` uses the set's.
    #[test]
    fn flythrough_frames_are_spaced_by_eye_distance() {
        let set = parse_views(&with_path(
            "(eye: (0.0, 1.0, 0.0), target: (0.0, 1.0, -5.0), fov_y_deg: 40.0, ev100: Some(14.0)), \
             (eye: (3.0, 1.0, 0.0), target: (3.0, 1.0, -5.0), fov_y_deg: 70.0), \
             (eye: (3.0, 1.0, 1.0), target: (8.0, 1.0, 1.0), fov_y_deg: 70.0, ev100: Some(12.0))",
        ))
        .expect("parses");
        let frames = set.flythrough_views(5);
        assert_eq!(frames.len(), 5);
        let xs: Vec<f32> = frames.iter().map(|v| v.eye[0]).collect();
        assert_eq!(xs, [0.0, 1.0, 2.0, 3.0, 3.0]);
        assert_eq!(frames[3].eye, [3.0, 1.0, 0.0]);
        assert_eq!(frames[4].eye, [3.0, 1.0, 1.0]);
        assert_eq!(frames[4].target, [8.0, 1.0, 1.0]);
        assert!((frames[1].fov_y_deg - 50.0).abs() < 1e-4);
        assert_eq!(frames[0].ev100, Some(14.0));
        assert!((frames[1].ev100.expect("ev") - (14.0 - 4.0 / 3.0)).abs() < 1e-4);
        assert_eq!(frames[3].ev100, Some(10.0), "the middle key has the set's ev100");
        assert_eq!(frames[4].ev100, Some(12.0));
        let names: Vec<&str> = frames.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["fly00", "fly01", "fly02", "fly03", "fly04"]);
        assert_eq!(set.flythrough_views(1)[0].eye, [0.0, 1.0, 0.0]);
        assert!(set.flythrough_views(0).is_empty());
    }

    /// A pan (the eye never moves) is spaced by key instead of dividing by a zero length.
    #[test]
    fn a_pan_is_spaced_by_key() {
        let set = parse_views(&with_path(
            "(eye: (0.0, 1.0, 0.0), target: (0.0, 1.0, -5.0), fov_y_deg: 60.0), \
             (eye: (0.0, 1.0, 0.0), target: (5.0, 1.0, 0.0), fov_y_deg: 60.0)",
        ))
        .expect("parses");
        let frames = set.flythrough_views(3);
        assert_eq!(frames[0].target, [0.0, 1.0, -5.0]);
        assert_eq!(frames[1].target, [2.5, 1.0, -2.5]);
        assert_eq!(frames[2].target, [5.0, 1.0, 0.0]);
        assert!(frames.iter().all(|v| v.eye == [0.0, 1.0, 0.0]));
    }

    /// One key, a bad key camera, a bad exposure, and a path that turns to look straight down
    /// between two good keys are all `views_flythrough`.
    #[test]
    fn bad_paths_are_refused() {
        let cases = [
            "(eye: (0.0, 1.0, 0.0), target: (0.0, 1.0, -5.0), fov_y_deg: 60.0)",
            "(eye: (0.0, 1.0, 0.0), target: (0.0, 1.0, -5.0), fov_y_deg: 60.0), \
             (eye: (0.0, 1.0, 0.0), target: (0.0, 1.0, 0.0), fov_y_deg: 60.0)",
            "(eye: (0.0, 1.0, 0.0), target: (0.0, 1.0, -5.0), fov_y_deg: 60.0), \
             (eye: (1.0, 1.0, 0.0), target: (1.0, 1.0, -5.0), fov_y_deg: 200.0)",
            "(eye: (0.0, 1.0, 0.0), target: (0.0, 1.0, -5.0), fov_y_deg: 60.0, ev100: Some(inf)), \
             (eye: (1.0, 1.0, 0.0), target: (1.0, 1.0, -5.0), fov_y_deg: 60.0)",
            // Halfway, the target sits straight below the eye.
            "(eye: (0.0, 5.0, 0.0), target: (-1.0, 0.0, 0.0), fov_y_deg: 60.0), \
             (eye: (0.0, 5.0, 0.0), target: (1.0, 0.0, 0.0), fov_y_deg: 60.0)",
        ];
        for keys in cases {
            let err = parse_views(&with_path(keys)).expect_err(keys);
            assert_eq!(err.code(), "views_flythrough", "{keys}: {err}");
        }
    }
}
