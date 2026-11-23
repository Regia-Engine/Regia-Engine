// SPDX-License-Identifier: MPL-2.0
//! Screen-size LOD thresholds and the hysteresis selector shared by the cooker and the renderer.
//!
//! Generated LODs store the projected size at which the next coarser level's simplification
//! error is one pixel on a 1080p display (ADR-208). The selector is the same test terrain
//! scatter uses: the first level whose threshold is at most the projected size, held inside a
//! ±15 % band, with an optional cross-fade.

/// Viewport height the cooker assumes when it turns a geometric error into a screen size.
pub const LOD_SCREEN_REFERENCE_HEIGHT: f32 = 1080.0;

/// Screen sizes written when a pre-metric multi-LOD payload is decoded.
///
/// The same curve terrain scatter uses when a cooked LOD stored no positive size
/// (`0.06`, `0.018`, `0.003`, `0`). The last entry is always forced to 0.
pub const LEGACY_LOD_SCREEN_SIZES: [f32; 4] = [0.06, 0.018, 0.003, 0.0];

/// [`crate::CookedMesh::lod_metric`]: sizes came from a cooker that stored triangle ratios
/// (or a single LOD left as it was).
pub const LOD_METRIC_LEGACY: u32 = 0;

/// [`crate::CookedMesh::lod_metric`]: sizes are [`screen_size_for_error`] of the simplifier.
pub const LOD_METRIC_ERROR: u32 = 1;

/// [`crate::CookedMesh::lod_metric`]: a multi-LOD legacy payload rewritten to
/// [`LEGACY_LOD_SCREEN_SIZES`] on decode. The geometric error is not in the old bytes.
pub const LOD_METRIC_MIGRATED: u32 = 2;

/// [`LodSwitch::lod`] before the selector has seen the instance.
pub const LOD_UNSET: u8 = u8::MAX;

/// Minimum absolute error (metres) treated as a real simplification error.
const MIN_ERROR_M: f32 = 1.0e-6;

/// Largest stored screen-size threshold. A zero or non-finite error maps here, so the
/// coarser level is chosen for every practical projected size.
const MAX_SCREEN_SIZE: f32 = 1.0e6;

/// Projected screen size at which `error_abs` (metres) covers one pixel of a
/// [`LOD_SCREEN_REFERENCE_HEIGHT`] display.
///
/// `screen_size = diameter / (2 · distance · tan(fov_y / 2))` is 1 when the bounding diameter
/// fills the viewport height. The error in pixels is
/// `(error_abs / diameter) · screen_size · viewport_height`. At one pixel and 1080 rows,
/// `screen_size = diameter / (error_abs · 1080)`.
///
/// A non-finite error or diameter, a diameter that is zero or negative, or an error below
/// `MIN_ERROR_M` (1e-6 m; zero and negatives included) returns the cap (`1e6`; the coarse level
/// is used almost always). The formula is not applied to those inputs. Otherwise the threshold
/// is clamped to `[1e-6, 1e6]`. Past `diameter / (1e-6 · 1080)` a larger error stays at `1e-6`.
/// Below that, a larger error returns a smaller threshold (the fine level is kept longer).
#[must_use]
pub fn screen_size_for_error(error_abs: f32, diameter: f32) -> f32 {
    if !error_abs.is_finite() || !diameter.is_finite() || diameter <= 0.0 || error_abs < MIN_ERROR_M {
        return MAX_SCREEN_SIZE;
    }
    let size = diameter / (error_abs * LOD_SCREEN_REFERENCE_HEIGHT);
    if !size.is_finite() {
        return MAX_SCREEN_SIZE;
    }
    size.clamp(MIN_ERROR_M, MAX_SCREEN_SIZE)
}

/// Strictly decreasing screen-size thresholds for a LOD chain.
///
/// `errors_abs[i]` is the absolute simplification error (metres) of LOD `i + 1`. The returned
/// vector has one extra entry, the last LOD, which is always 0. LOD `i` (except the last)
/// stores [`screen_size_for_error`] of the next coarser level.
#[must_use]
pub fn thresholds_from_errors(errors_abs: &[f32], diameter: f32) -> Vec<f32> {
    let mut sizes: Vec<f32> = errors_abs
        .iter()
        .copied()
        .map(|error| screen_size_for_error(error, diameter))
        .collect();
    sizes.push(0.0);
    // A coarser level must switch at a smaller projected size. When two errors clamp to the
    // same cap, lower the coarser threshold instead of exceeding the cap.
    for i in 0..sizes.len().saturating_sub(1) {
        if sizes[i + 1] < sizes[i] {
            continue;
        }
        if i + 1 == sizes.len() - 1 {
            if sizes[i] <= 0.0 {
                sizes[i] = 1.0e-4;
            }
            continue;
        }
        let lowered = sizes[i] * 0.5;
        sizes[i + 1] = if lowered.is_finite() {
            lowered.max(0.0)
        } else {
            0.0
        };
    }
    if let Some(last) = sizes.last_mut() {
        *last = 0.0;
    }
    sizes
}

/// Legacy screen sizes for `count` levels. The last is 0. Counts below 2 are not rewritten
/// by the migrator; this still returns `[0]` for a single level.
#[must_use]
pub fn legacy_screen_sizes(count: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        if i + 1 == count {
            out.push(0.0);
        } else {
            out.push({
                // Past the four-entry curve keep halving so thresholds stay strictly decreasing.
                let base = LEGACY_LOD_SCREEN_SIZES[i.min(2)];
                base * 0.5f32.powi(i.saturating_sub(2).min(64) as i32)
            });
        }
    }
    out
}

/// Scale applied to a stored threshold at runtime so a taller viewport keeps the fine LOD
/// longer: `stored * (1080 / height)`. An unknown height uses 1080 (scale 1).
#[must_use]
pub fn lod_threshold_scale(viewport_height: u32) -> f32 {
    LOD_SCREEN_REFERENCE_HEIGHT / viewport_height.max(1) as f32
}

/// Projected screen size of a sphere given its **radius** (not its diameter): the bounding
/// diameter over the viewport height.
///
/// `radius / (distance · tan(fov_y / 2))`. A non-positive radius is 0. Distance is clamped
/// away from 0.
#[must_use]
pub fn projected_screen_size(radius: f32, distance: f32, fov_y: f32) -> f32 {
    if !radius.is_finite() || radius <= 0.0 {
        return 0.0;
    }
    let dist = if distance.is_finite() {
        distance.max(1.0e-3)
    } else {
        1.0e-3
    };
    // A half-angle at or past 90 degrees has a negative or infinite tangent; clamp it so a wide
    // field of view reads as a very wide view (small objects), never as a tiny one.
    let half = if fov_y.is_finite() {
        (fov_y * 0.5).clamp(0.0, std::f32::consts::FRAC_PI_2 - 1.0e-3)
    } else {
        0.0
    };
    let tan_half = half.tan().max(1.0e-4);
    let size = radius / (dist * tan_half);
    if size.is_finite() { size } else { 0.0 }
}

/// The LOD drawn at projected `size`: the first threshold `size` still meets, else the last.
#[must_use]
pub fn lod_at(screen_sizes: &[f32], size: f32) -> usize {
    if screen_sizes.is_empty() {
        return 0;
    }
    screen_sizes
        .iter()
        .position(|threshold| size >= *threshold)
        .unwrap_or(screen_sizes.len() - 1)
}

/// Hysteresis memory for one selector (colour or shadow).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LodSwitch {
    /// LOD drawn this frame (the incoming LOD while a fade runs).
    pub lod: u8,
    /// LOD drawn before the current switch. Equal to [`Self::lod`] when settled.
    pub prev: u8,
    /// Frame the current switch started.
    pub start: u64,
    /// Last frame this selector ran. A gap means the instance returned to view.
    pub seen: u64,
}

impl Default for LodSwitch {
    fn default() -> Self {
        Self {
            lod: LOD_UNSET,
            prev: LOD_UNSET,
            start: 0,
            seen: 0,
        }
    }
}

/// The LOD to draw, and the outgoing LOD plus the incoming LOD's pixel share while a
/// cross-fade runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LodPick {
    /// Incoming LOD index.
    pub lod: usize,
    /// `(outgoing LOD, incoming coverage)` during a cross-fade. Coverage is
    /// `(k + 1) / (fade_frames + 1)` on frame `k` of the fade.
    pub fade: Option<(usize, f32)>,
}

/// Advance `state` one frame and return the LOD to draw.
///
/// Mirrors terrain scatter: off screen, or back after a skipped frame, a switch is immediate.
/// While settled, the current LOD is kept when it still lies between the LODs of
/// `size · (1 ± hysteresis)`. `crossfade` false (shadows) switches in one frame.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn step_lod(
    screen_sizes: &[f32],
    size: f32,
    on_screen: bool,
    crossfade: bool,
    hysteresis: f32,
    fade_frames: u64,
    frame: u64,
    state: &mut LodSwitch,
) -> LodPick {
    if screen_sizes.is_empty() {
        return LodPick { lod: 0, fade: None };
    }
    let returning = state.seen.saturating_add(1) < frame;
    if state.lod == LOD_UNSET || usize::from(state.lod) >= screen_sizes.len() || returning {
        let lod = lod_at(screen_sizes, size);
        *state = LodSwitch {
            lod: lod as u8,
            prev: lod as u8,
            start: frame,
            seen: frame,
        };
        return LodPick { lod, fade: None };
    }
    state.seen = frame;
    let elapsed = frame.saturating_sub(state.start);
    if state.prev != state.lod && (elapsed >= fade_frames || !(crossfade && on_screen)) {
        state.prev = state.lod;
    }
    if state.prev == state.lod {
        let band = hysteresis.abs();
        let finest = lod_at(screen_sizes, size * (1.0 + band));
        let coarsest = lod_at(screen_sizes, size * (1.0 - band));
        let current = usize::from(state.lod);
        let target = current.clamp(finest, coarsest);
        if target != current {
            state.prev = if crossfade && on_screen {
                state.lod
            } else {
                target as u8
            };
            state.lod = target as u8;
            state.start = frame;
        }
    }
    if state.prev == state.lod {
        return LodPick {
            lod: usize::from(state.lod),
            fade: None,
        };
    }
    let span = fade_frames.max(1);
    let k = frame.saturating_sub(state.start).min(span - 1);
    let coverage = (k + 1) as f32 / (span + 1) as f32;
    LodPick {
        lod: usize::from(state.lod),
        fade: Some((usize::from(state.prev), coverage)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_half_angle_past_90_degrees_never_reads_as_a_tiny_view() {
        let sane = projected_screen_size(1.0, 10.0, 1.0);
        let wide = projected_screen_size(1.0, 10.0, 4.0);
        assert!(wide.is_finite() && wide >= 0.0);
        // Before the fix this was 1000 (negative tangent replaced by 1e-4).
        assert!(wide < sane, "wide {wide} sane {sane}");
        assert!(projected_screen_size(1.0, 10.0, 7.0) < sane);
    }

    #[test]
    fn screen_size_shrinks_as_the_error_grows() {
        let fine = screen_size_for_error(0.01, 2.0);
        let coarse = screen_size_for_error(0.1, 2.0);
        assert!((fine - 2.0 / (0.01 * 1080.0)).abs() < 1.0e-4);
        assert!(fine > coarse);
        assert_eq!(screen_size_for_error(0.0, 2.0), MAX_SCREEN_SIZE);
        assert_eq!(screen_size_for_error(f32::NAN, 2.0), MAX_SCREEN_SIZE);
        assert_eq!(screen_size_for_error(0.01, 0.0), MAX_SCREEN_SIZE);
        // Both sit under the 1e-6 clamp, so the larger error does not shrink further.
        assert_eq!(screen_size_for_error(1000.0, 1.0), MIN_ERROR_M);
        assert_eq!(screen_size_for_error(10000.0, 1.0), MIN_ERROR_M);
    }

    #[test]
    fn thresholds_are_strictly_decreasing_and_end_at_zero() {
        let sizes = thresholds_from_errors(&[0.0, 0.05, 0.2], 4.0);
        assert_eq!(sizes.len(), 4);
        assert_eq!(*sizes.last().expect("last"), 0.0);
        for pair in sizes.windows(2) {
            assert!(pair[0] > pair[1], "{sizes:?}");
        }
    }

    #[test]
    fn legacy_curve_matches_scatter_defaults() {
        assert_eq!(legacy_screen_sizes(2), vec![0.06, 0.0]);
        assert_eq!(legacy_screen_sizes(3), vec![0.06, 0.018, 0.0]);
        assert_eq!(legacy_screen_sizes(4), vec![0.06, 0.018, 0.003, 0.0]);
    }

    #[test]
    fn legacy_curve_stays_strictly_decreasing_for_long_chains() {
        for count in 2..12 {
            let sizes = legacy_screen_sizes(count);
            assert_eq!(sizes.len(), count);
            assert_eq!(*sizes.last().expect("last"), 0.0);
            for pair in sizes.windows(2) {
                assert!(pair[0] > pair[1], "{sizes:?}");
            }
        }
    }

    #[test]
    fn distance_picks_a_coarser_lod() {
        let sizes = [0.5, 0.1, 0.0];
        assert_eq!(lod_at(&sizes, 0.8), 0);
        assert_eq!(lod_at(&sizes, 0.2), 1);
        assert_eq!(lod_at(&sizes, 0.01), 2);
    }

    #[test]
    fn hysteresis_holds_inside_the_band_and_switches_outside_it() {
        let sizes = [0.5, 0.0];
        let mut state = LodSwitch::default();
        let first = step_lod(&sizes, 0.6, true, true, 0.15, 8, 1, &mut state);
        assert_eq!(first.lod, 0);
        assert!(first.fade.is_none());
        // 0.45 is inside ±15 % of 0.5 (the edge is 0.5 / 1.15 ≈ 0.435 from above,
        // and the held LOD's coarsest probe is size · 0.85).
        let held = step_lod(&sizes, 0.46, true, true, 0.15, 8, 2, &mut state);
        assert_eq!(held.lod, 0, "size inside the band must not flicker");
        assert!(held.fade.is_none());
        let switched = step_lod(&sizes, 0.30, true, true, 0.15, 8, 3, &mut state);
        assert_eq!(switched.lod, 1);
        assert_eq!(switched.fade.map(|f| f.0), Some(0));
    }

    #[test]
    fn cross_fade_ramps_then_settles_and_off_screen_is_immediate() {
        let sizes = [0.5, 0.0];
        let mut state = LodSwitch::default();
        let _ = step_lod(&sizes, 1.0, true, true, 0.15, 8, 1, &mut state);
        for frame in 2..10 {
            let pick = step_lod(&sizes, 0.2, true, true, 0.15, 8, frame, &mut state);
            let coverage = pick.fade.expect("fading").1;
            let k = frame - 2;
            assert!((coverage - (k + 1) as f32 / 9.0).abs() < 1.0e-5);
        }
        let done = step_lod(&sizes, 0.2, true, true, 0.15, 8, 10, &mut state);
        assert_eq!(done.lod, 1);
        assert!(done.fade.is_none());

        let mut hard = LodSwitch::default();
        let _ = step_lod(&sizes, 1.0, true, false, 0.15, 8, 1, &mut hard);
        let jump = step_lod(&sizes, 0.2, false, true, 0.15, 8, 2, &mut hard);
        assert_eq!(jump.lod, 1);
        assert!(jump.fade.is_none(), "off screen switches immediately");
    }

    #[test]
    fn a_skipped_frame_drops_the_stale_lod() {
        let sizes = [0.5, 0.0];
        let mut state = LodSwitch::default();
        let _ = step_lod(&sizes, 1.0, true, true, 0.15, 8, 1, &mut state);
        let back = step_lod(&sizes, 0.2, true, true, 0.15, 8, 3, &mut state);
        assert_eq!(back.lod, 1);
        assert!(back.fade.is_none(), "returning from off screen does not fade");
    }
}
