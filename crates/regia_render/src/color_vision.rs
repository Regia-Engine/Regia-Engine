// SPDX-License-Identifier: MPL-2.0
//! Color-vision filters (ADR-2646, `book/agents/color-vision.md`): a simulation of protanopia,
//! deuteranopia and tritanopia for designers, and a compensation (daltonization) mode for players,
//! both delivered as a 3D LUT through the post stack's existing LUT step.
//!
//! **Off by default.** With [`ColorVisionMode::Off`] nothing is composed and every view keeps its own
//! LUT (or none), byte for byte.
//!
//! # Model
//!
//! - **Simulation**: the physiologically based model of Machado, Oliveira and Fernandes, "A
//!   Physiologically-based Model for Simulation of Color Vision Deficiency", IEEE TVCG 15(6), 2009
//!   (<https://www.inf.ufrgs.br/~oliveira/pubs_files/CVD_Simulation/CVD_Simulation.html>). Their
//!   published 3×3 matrices for severity 1.0 act on linear RGB; a lower severity is approximated
//!   here by blending the matrix with the identity, `M(s) = (1 − s)·I + s·M` (the paper tabulates
//!   intermediate severities; the linear blend is a simplification).
//! - **Compensation**: error redistribution after Fidaner, Lin and Ozguven, "Analysis of Color
//!   Blindness" (Stanford, 2005): the part of the color the player cannot see, `e = c − M(s)·c`, is
//!   added back into channels they can see. For protan and deutan the red error moves into green and
//!   blue (`Δg = 0.7·e_r + e_g`, `Δb = 0.7·e_r + e_b`); for tritan, by the same construction, the
//!   blue error moves into red and green (`Δr = e_r + 0.7·e_b`, `Δg = e_g + 0.7·e_b`).
//!
//! The LUT step samples display-encoded sRGB after the tonemap and the grade, so each entry is
//! decoded to linear, transformed, clamped to `0..=1` and encoded again.
//!
//! # Delivery
//!
//! [`apply_color_vision`] runs in `Render` right after `extract_frame`: for every extracted view it
//! composes the filter over the view's final LUT (the level look's LUT and the post-volume blend
//! are already in it) on that LUT's grid, or builds a standalone [`ColorVision::LUT_SIZE`]³ table when
//! the view has none. The last [`MAX_CACHED`] tables are kept by a key of their inputs, so a steady
//! frame composes nothing. The UI is drawn after the post stack and is not filtered.

use std::sync::Arc;

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::lut::ColorLut;

/// The kind of color-vision deficiency a filter models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorVisionDeficiency {
    /// Missing or anomalous long-wavelength (red) cones.
    Protanopia,
    /// Missing or anomalous medium-wavelength (green) cones; the most common kind.
    #[default]
    Deuteranopia,
    /// Missing or anomalous short-wavelength (blue) cones.
    Tritanopia,
}

impl ColorVisionDeficiency {
    /// `protanopia`, `deuteranopia` or `tritanopia`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Protanopia => "protanopia",
            Self::Deuteranopia => "deuteranopia",
            Self::Tritanopia => "tritanopia",
        }
    }

    /// Parses [`ColorVisionDeficiency::as_str`] (ignoring ASCII case).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "protanopia" => Some(Self::Protanopia),
            "deuteranopia" => Some(Self::Deuteranopia),
            "tritanopia" => Some(Self::Tritanopia),
            _ => None,
        }
    }

    /// The severity-1.0 simulation matrix on linear RGB (Machado, Oliveira and Fernandes 2009),
    /// rows = output channel.
    #[must_use]
    pub fn matrix(self) -> [[f32; 3]; 3] {
        match self {
            Self::Protanopia => [
                [0.152_286, 1.052_583, -0.204_868],
                [0.114_503, 0.786_281, 0.099_216],
                [-0.003_882, -0.048_116, 1.051_998],
            ],
            Self::Deuteranopia => [
                [0.367_322, 0.860_646, -0.227_968],
                [0.280_085, 0.672_501, 0.047_413],
                [-0.011_820, 0.042_940, 0.968_881],
            ],
            Self::Tritanopia => [
                [1.255_528, -0.076_749, -0.178_779],
                [-0.078_411, 0.930_809, 0.147_602],
                [0.004_733, 0.691_367, 0.303_900],
            ],
        }
    }
}

/// What the filter does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorVisionMode {
    /// No filter (the default).
    #[default]
    Off,
    /// Show the image as a viewer with the deficiency would see it (a design check).
    Simulate,
    /// Shift colors the viewer cannot tell apart into ones they can (a player option).
    Compensate,
}

impl ColorVisionMode {
    /// `off`, `simulate` or `compensate`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Simulate => "simulate",
            Self::Compensate => "compensate",
        }
    }

    /// Parses [`ColorVisionMode::as_str`] (ignoring ASCII case).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "simulate" => Some(Self::Simulate),
            "compensate" => Some(Self::Compensate),
            _ => None,
        }
    }
}

/// A color-vision filter: mode, deficiency and severity. The default is off.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ColorVisionFilter {
    /// Off, simulate or compensate.
    #[serde(default)]
    pub mode: ColorVisionMode,
    /// Which cones the filter models.
    #[serde(default)]
    pub deficiency: ColorVisionDeficiency,
    /// `0` (no effect) to `1` (full dichromacy); NaN reads as 1.
    #[serde(default = "full_severity")]
    pub severity: f32,
}

fn full_severity() -> f32 {
    1.0
}

impl Default for ColorVisionFilter {
    fn default() -> Self {
        Self {
            mode: ColorVisionMode::Off,
            deficiency: ColorVisionDeficiency::default(),
            severity: 1.0,
        }
    }
}

impl ColorVisionFilter {
    /// A filter of `mode` for `deficiency` at full severity.
    #[must_use]
    pub fn new(mode: ColorVisionMode, deficiency: ColorVisionDeficiency) -> Self {
        Self {
            mode,
            deficiency,
            severity: 1.0,
        }
    }

    /// The severity in `0..=1` (NaN reads as 1).
    #[must_use]
    pub fn severity(&self) -> f32 {
        if self.severity.is_nan() {
            1.0
        } else {
            self.severity.clamp(0.0, 1.0)
        }
    }

    /// Whether the filter changes nothing (off, or a severity of 0).
    #[must_use]
    pub fn is_off(&self) -> bool {
        self.mode == ColorVisionMode::Off || self.severity() == 0.0
    }

    /// The simulation matrix at this filter's severity: `(1 − s)·I + s·M`.
    #[must_use]
    pub fn matrix(&self) -> [[f32; 3]; 3] {
        let s = self.severity();
        let m = self.deficiency.matrix();
        let mut out = [[0.0; 3]; 3];
        for (row, (out_row, m_row)) in out.iter_mut().zip(m).enumerate() {
            for (col, (o, v)) in out_row.iter_mut().zip(m_row).enumerate() {
                let identity = if row == col { 1.0 } else { 0.0 };
                *o = identity * (1.0 - s) + v * s;
            }
        }
        out
    }

    /// Applies the filter to a linear RGB color (not clamped).
    #[must_use]
    pub fn apply_linear(&self, c: [f32; 3]) -> [f32; 3] {
        if self.is_off() {
            return c;
        }
        let sim = mul(&self.matrix(), c);
        match self.mode {
            ColorVisionMode::Off => c,
            ColorVisionMode::Simulate => sim,
            ColorVisionMode::Compensate => {
                let e = [c[0] - sim[0], c[1] - sim[1], c[2] - sim[2]];
                let shift = match self.deficiency {
                    ColorVisionDeficiency::Protanopia | ColorVisionDeficiency::Deuteranopia => {
                        [0.0, 0.7 * e[0] + e[1], 0.7 * e[0] + e[2]]
                    }
                    ColorVisionDeficiency::Tritanopia => [e[0] + 0.7 * e[2], e[1] + 0.7 * e[2], 0.0],
                };
                [c[0] + shift[0], c[1] + shift[1], c[2] + shift[2]]
            }
        }
    }

    /// Applies the filter to a display-encoded sRGB color in `0..=1` (the LUT step's space): decode,
    /// [`ColorVisionFilter::apply_linear`], clamp, encode.
    #[must_use]
    pub fn apply_display(&self, c: [f32; 3]) -> [f32; 3] {
        if self.is_off() {
            return c;
        }
        let linear = c.map(|v| regia_core::math::srgb_to_linear(v.clamp(0.0, 1.0)));
        self.apply_linear(linear)
            .map(|v| regia_core::math::linear_to_srgb(v.clamp(0.0, 1.0)))
    }

    /// The filter as a standalone LUT of edge `size`.
    #[must_use]
    pub fn lut(&self, size: u32) -> ColorLut {
        ColorLut::from_fn(size, |c| self.apply_display(c))
    }

    /// The filter applied after `base` (a view's LUT): every entry of `base` goes through the
    /// filter, on `base`'s grid. Without a base it is [`ColorVisionFilter::lut`] at
    /// [`ColorVision::LUT_SIZE`]. `None` when the filter is off (the view keeps `base`).
    #[must_use]
    pub fn compose(&self, base: Option<&ColorLut>) -> Option<ColorLut> {
        if self.is_off() {
            return None;
        }
        match base {
            None => Some(self.lut(ColorVision::LUT_SIZE)),
            Some(base) => {
                let data = base.data().iter().map(|&c| self.apply_display(c)).collect();
                ColorLut::from_entries(base.size(), data)
            }
        }
    }

    /// A key of everything the composed table depends on, with `base` the view's LUT key.
    fn key(&self, base: Option<u64>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.mode.hash(&mut hasher);
        self.deficiency.hash(&mut hasher);
        self.severity().to_bits().hash(&mut hasher);
        base.hash(&mut hasher);
        hasher.finish()
    }
}

fn mul(m: &[[f32; 3]; 3], c: [f32; 3]) -> [f32; 3] {
    m.map(|row| row[0] * c[0] + row[1] * c[1] + row[2] * c[2])
}

/// Composed tables kept by [`ColorVision`].
pub const MAX_CACHED: usize = 4;

/// Counters for tests and the agent protocol.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColorVisionStats {
    /// Tables composed (cache misses).
    pub composed: u64,
    /// Views served from the cache.
    pub cache_hits: u64,
    /// Views filtered on the last frame.
    pub views_last_frame: u32,
}

/// The active color-vision filter and its table cache. The framework copies
/// `GameSettings.accessibility.color_vision` into [`ColorVision::filter`].
#[derive(Resource, Debug, Default)]
pub struct ColorVision {
    /// The filter; off by default.
    pub filter: ColorVisionFilter,
    cache: Vec<(u64, Arc<ColorLut>)>,
    stats: ColorVisionStats,
}

impl ColorVision {
    /// The edge of the standalone table used when a view has no LUT of its own.
    pub const LUT_SIZE: u32 = 33;

    /// A resource holding `filter`.
    #[must_use]
    pub fn new(filter: ColorVisionFilter) -> Self {
        Self {
            filter,
            ..Self::default()
        }
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> ColorVisionStats {
        self.stats
    }

    /// The LUT a view with `base` should use: `base` itself while the filter is off, else the
    /// composed table (cached by its inputs).
    pub fn view_lut(&mut self, base: Option<&Arc<ColorLut>>) -> Option<Arc<ColorLut>> {
        if self.filter.is_off() {
            return base.cloned();
        }
        let key = self.filter.key(base.map(|l| l.key()));
        if let Some(i) = self.cache.iter().position(|(k, _)| *k == key) {
            let hit = self.cache.remove(i);
            let lut = hit.1.clone();
            self.cache.push(hit);
            self.stats.cache_hits += 1;
            return Some(lut);
        }
        let _span = tracing::info_span!("color_vision_compose").entered();
        let Some(lut) = self.filter.compose(base.map(Arc::as_ref)).map(Arc::new) else {
            return base.cloned();
        };
        self.stats.composed += 1;
        if self.cache.len() >= MAX_CACHED {
            self.cache.remove(0);
        }
        self.cache.push((key, lut.clone()));
        Some(lut)
    }
}

/// `Render`, after `extract_frame`: puts the color-vision filter into every extracted view's LUT.
/// Does nothing while the filter is off or the resource is missing.
pub fn apply_color_vision(
    mut extracted: ResMut<crate::runtime::ExtractedFrame>,
    vision: Option<ResMut<ColorVision>>,
) {
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::COLOR_LUTS);
    let Some(mut vision) = vision else {
        return;
    };
    if vision.filter.is_off() {
        if vision.stats.views_last_frame != 0 {
            vision.stats.views_last_frame = 0;
        }
        return;
    }
    let mut views = 0u32;
    for view in &mut extracted.views {
        view.post.lut = vision.view_lut(view.post.lut.as_ref());
        views += 1;
    }
    vision.stats.views_last_frame = views;
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ColorVisionDeficiency; 3] = [
        ColorVisionDeficiency::Protanopia,
        ColorVisionDeficiency::Deuteranopia,
        ColorVisionDeficiency::Tritanopia,
    ];

    fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }

    #[test]
    fn off_is_the_identity_and_keeps_the_view_lut() {
        let off = ColorVisionFilter::default();
        assert!(off.is_off());
        for c in [[0.0, 0.0, 0.0], [0.2, 0.5, 0.9], [1.0, 1.0, 1.0]] {
            assert_eq!(off.apply_display(c), c);
        }
        assert!(off.compose(None).is_none());
        // Severity 0 is off too, whatever the mode.
        let none = ColorVisionFilter {
            severity: 0.0,
            ..ColorVisionFilter::new(ColorVisionMode::Simulate, ColorVisionDeficiency::Protanopia)
        };
        assert!(none.is_off());
        // The resource hands the view's own table back untouched.
        let base = Arc::new(ColorLut::from_fn(9, |c| [c[0] * 0.5, c[1], c[2]]));
        let mut vision = ColorVision::default();
        let out = vision.view_lut(Some(&base)).expect("kept");
        assert!(Arc::ptr_eq(&out, &base));
        assert!(vision.view_lut(None).is_none());
        assert_eq!(vision.stats().composed, 0);
    }

    #[test]
    fn the_published_matrices_keep_white_and_gray() {
        for d in ALL {
            for row in d.matrix() {
                let sum: f32 = row.iter().sum();
                assert!((sum - 1.0).abs() < 1e-5, "{d:?} row sums to {sum}");
            }
            for mode in [ColorVisionMode::Simulate, ColorVisionMode::Compensate] {
                let filter = ColorVisionFilter::new(mode, d);
                for v in [0.0, 0.18, 0.5, 1.0] {
                    let out = filter.apply_display([v, v, v]);
                    assert!(
                        dist(out, [v, v, v]) < 2e-3,
                        "{mode:?} {d:?} moved gray {v} to {out:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn red_and_green_collapse_to_one_hue_for_protan_and_deutan_but_not_tritan() {
        // Chromaticity r / (r + g) of the simulated linear color.
        let hue = |d: ColorVisionDeficiency, c: [f32; 3]| {
            let s = ColorVisionFilter::new(ColorVisionMode::Simulate, d)
                .apply_linear(c)
                .map(|v| v.max(0.0));
            s[0] / (s[0] + s[1])
        };
        let red = [1.0, 0.0, 0.0];
        let green = [0.0, 1.0, 0.0];
        for d in [
            ColorVisionDeficiency::Protanopia,
            ColorVisionDeficiency::Deuteranopia,
        ] {
            let gap = (hue(d, red) - hue(d, green)).abs();
            assert!(gap < 0.05, "{d:?}: red and green should look alike (gap {gap})");
        }
        let gap = (hue(ColorVisionDeficiency::Tritanopia, red)
            - hue(ColorVisionDeficiency::Tritanopia, green))
        .abs();
        assert!(gap > 0.8, "tritanopia keeps red and green apart (gap {gap})");
    }

    /// The unit direction the simulation shrinks most: the eigenvector of `MᵀM` with the
    /// smallest eigenvalue, by power iteration on `trace(MᵀM)·I − MᵀM`.
    fn least_visible_direction(m: [[f32; 3]; 3]) -> [f32; 3] {
        let m = m.map(|row| row.map(f64::from));
        let mut a = [[0.0f64; 3]; 3];
        for (i, a_row) in a.iter_mut().enumerate() {
            for (j, v) in a_row.iter_mut().enumerate() {
                *v = (0..3).map(|k| m[k][i] * m[k][j]).sum();
            }
        }
        let trace = a[0][0] + a[1][1] + a[2][2];
        let mut x = [1.0f64, 0.7, 0.3];
        for _ in 0..200 {
            let y: [f64; 3] =
                std::array::from_fn(|i| trace * x[i] - (a[i][0] * x[0] + a[i][1] * x[1] + a[i][2] * x[2]));
            let len = (y[0] * y[0] + y[1] * y[1] + y[2] * y[2]).sqrt();
            x = y.map(|v| v / len);
        }
        x.map(|v| v as f32)
    }

    #[test]
    fn compensation_separates_a_pair_the_simulation_confuses() {
        for d in ALL {
            // Two colors around mid gray that differ along the direction the viewer sees least:
            // for protan and deutan the matrix is rank 2 and the pair looks identical; the
            // tritan matrix keeps about a sixth of the difference.
            let n = least_visible_direction(d.matrix()).map(|v| v * 0.15);
            let a = [0.4 - n[0], 0.4 - n[1], 0.4 - n[2]];
            let b = [0.4 + n[0], 0.4 + n[1], 0.4 + n[2]];
            assert!(
                a.iter().chain(&b).all(|v| (0.0..=1.0).contains(v)),
                "{d:?} pair in gamut"
            );
            let original = dist(a, b);
            let simulate = ColorVisionFilter::new(ColorVisionMode::Simulate, d);
            let compensate = ColorVisionFilter::new(ColorVisionMode::Compensate, d);
            let seen = dist(simulate.apply_linear(a), simulate.apply_linear(b));
            assert!(
                seen < 0.2 * original,
                "{d:?}: the pair is a confusion pair ({seen} of {original})"
            );
            let helped = dist(
                simulate.apply_linear(compensate.apply_linear(a)),
                simulate.apply_linear(compensate.apply_linear(b)),
            );
            assert!(
                helped > 4.0 * seen.max(1e-3) && helped > 0.5 * original,
                "{d:?}: compensated pair {helped} vs {seen} (of {original})"
            );
        }
    }

    #[test]
    fn severity_blends_from_identity_to_the_full_matrix() {
        let c = [0.9, 0.2, 0.1];
        let full = ColorVisionFilter::new(ColorVisionMode::Simulate, ColorVisionDeficiency::Deuteranopia);
        let half = ColorVisionFilter {
            severity: 0.5,
            ..full
        };
        let f = full.apply_linear(c);
        let h = half.apply_linear(c);
        for ((h, c), f) in h.iter().zip(c).zip(f) {
            assert!((h - (c + f) * 0.5).abs() < 1e-6);
        }
        let nan = ColorVisionFilter {
            severity: f32::NAN,
            ..full
        };
        assert_eq!(nan.severity(), 1.0);
    }

    #[test]
    fn the_lut_matches_the_filter_and_composes_over_a_view_lut() {
        let filter = ColorVisionFilter::new(ColorVisionMode::Compensate, ColorVisionDeficiency::Protanopia);
        let lut = filter.lut(ColorVision::LUT_SIZE);
        assert_eq!(lut.size(), ColorVision::LUT_SIZE);
        // Grid points are exact.
        let corner = lut.data()[(ColorVision::LUT_SIZE - 1) as usize];
        assert_eq!(corner, filter.apply_display([1.0, 0.0, 0.0]));
        // Composed over a base, each entry is the filter of the base's entry, on the base grid.
        let base = ColorLut::from_fn(5, |c| [c[1], c[0], c[2]]);
        let composed = filter.compose(Some(&base)).expect("on");
        assert_eq!(composed.size(), 5);
        for (out, input) in composed.data().iter().zip(base.data()) {
            assert_eq!(*out, filter.apply_display(*input));
        }
    }

    #[test]
    fn the_resource_caches_and_rebuilds_when_an_input_changes() {
        let mut vision = ColorVision::new(ColorVisionFilter::new(
            ColorVisionMode::Simulate,
            ColorVisionDeficiency::Tritanopia,
        ));
        let first = vision.view_lut(None).expect("on");
        let again = vision.view_lut(None).expect("on");
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!((vision.stats().composed, vision.stats().cache_hits), (1, 1));
        let base = Arc::new(ColorLut::identity(17));
        let over = vision.view_lut(Some(&base)).expect("on");
        assert_eq!(over.size(), 17);
        vision.filter.deficiency = ColorVisionDeficiency::Protanopia;
        let other = vision.view_lut(None).expect("on");
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(vision.stats().composed, 3);
    }

    #[test]
    fn the_system_filters_every_view_and_leaves_them_alone_when_off() {
        let mut world = World::new();
        let view = || crate::runtime::ExtractedView {
            camera: crate::runtime::ExtractedCamera {
                eye: [0.0, 1.0, 4.0],
                target: [0.0; 3],
                fov_y: 1.0,
                near: 0.1,
                far: 100.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: crate::runtime::ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: crate::PostSettings::default(),
        };
        let mut frame = crate::runtime::ExtractedFrame::default();
        frame.views.push(view());
        frame.views.push(view());
        let base = Arc::new(ColorLut::identity(9));
        frame.views[1].post.lut = Some(base.clone());
        world.insert_resource(frame);
        world.insert_resource(ColorVision::default());
        let mut schedule = Schedule::default();
        schedule.add_systems(apply_color_vision);
        schedule.run(&mut world);
        let views = &world.resource::<crate::runtime::ExtractedFrame>().views;
        assert!(views[0].post.lut.is_none());
        assert!(Arc::ptr_eq(views[1].post.lut.as_ref().expect("kept"), &base));

        world.resource_mut::<ColorVision>().filter =
            ColorVisionFilter::new(ColorVisionMode::Compensate, ColorVisionDeficiency::Deuteranopia);
        schedule.run(&mut world);
        let views = &world.resource::<crate::runtime::ExtractedFrame>().views;
        assert_eq!(
            views[0].post.lut.as_ref().map(|l| l.size()),
            Some(ColorVision::LUT_SIZE)
        );
        assert_eq!(views[1].post.lut.as_ref().map(|l| l.size()), Some(9));
        assert_eq!(world.resource::<ColorVision>().stats().views_last_frame, 2);
    }
}
