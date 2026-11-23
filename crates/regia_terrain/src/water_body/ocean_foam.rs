// SPDX-License-Identifier: MPL-2.0
//! Foam of the FFT ocean beyond the instant (ADR-2843, `docs/specs/water-ocean.md` § 7a): foam
//! that lingers after a crest has passed, and whitecaps that keep their share of the surface with
//! distance. Both are off by default ([`FftOcean::foam_decay`] 0, [`FftOcean::whitecap_rescale`]
//! false), so a default frame is the frame without this module.
//!
//! # Lingering foam
//!
//! The instantaneous foam is `crest_foam(J)` of the Jacobian at the instant (Tessendorf 2001):
//! it comes and goes with the crest. With [`FftOcean::foam_decay`] on, each band keeps a history
//! map on its own tile (one value per texel), stepped once per evolve of the maps:
//!
//! ```text
//! c = smoothstep(onset, onset + FOAM_RAMP, s_b (1 − J_b))      the band's own crest, normalized
//! H ← H + (c − H) g          when c ≥ H, g = 1 − exp(−Δt / foam_growth)   (growth)
//! H ← max(c, H − Δt / foam_decay)   otherwise                              (linear decay)
//! ```
//!
//! `J_b` is the Jacobian of the band alone, and `s_b` ([`band_foam_scales`]) divides its
//! compression by the band's share of the deviation of `1 − J` (the band's slope variance over
//! the sum's, square-rooted), so every band crosses the onset at the same number of deviations
//! as the summed surface does. The maps are indexed by the rest point, so the history moves with
//! the water's orbital motion and needs no advection step. Decay is linear, so foam left by a
//! crest reaches exactly zero `H / (Δt / foam_decay)` steps after the crest has gone.
//!
//! The drawn foam is `max(crest_foam(J), strength · max_b(fade_b H_b))`: lingering foam never
//! removes instantaneous foam. The history restarts on a step back in time, a step longer than
//! [`FOAM_MAX_STEP`], a new simulation (another ocean or grid), a change of the water bodies and a
//! camera cut. [`FoamHistory`] is the CPU mirror of the compute stage.
//!
//! # Whitecaps with distance
//!
//! The fragment shader fades each band on the pixel's footprint, and a faded band takes its share
//! of `1 − J` with it, so whitecaps thin out with distance. With [`FftOcean::whitecap_rescale`],
//! the drawn compression is scaled back up by the deviation the faded bands lost, under the
//! Gaussian model of the Jacobian's spread that Dupuy and Bruneton (2012) use for whitecap
//! coverage: `1 − J' = r (1 − J)`, `r = sqrt(Σ σ_b² / Σ fade_b² σ_b²)`, capped at
//! [`WHITECAP_RESCALE_MAX`]. [`whitecap_coverage`] measures the result.

use super::ocean::{BilinearCell, FftOcean, OCEAN_BANDS, OceanFrame, OceanGrid, OceanTables};
use super::swell::{FOAM_RAMP, crest_foam};

/// The longest [`FftOcean::foam_decay`] (s).
pub const FOAM_DECAY_MAX: f32 = 120.0;

/// The longest [`FftOcean::foam_growth`] (s).
pub const FOAM_GROWTH_MAX: f32 = 30.0;

/// The default [`FftOcean::foam_growth`] (s).
pub const DEFAULT_FOAM_GROWTH: f32 = 0.5;

/// The longest clock step the history integrates (s); a longer one restarts it, like a step back.
pub const FOAM_MAX_STEP: f64 = 1.0;

/// The most [`FftOcean::whitecap_rescale`] multiplies the drawn compression by.
pub const WHITECAP_RESCALE_MAX: f32 = 4.0;

/// The smallest share of the deviation a band is normalized by ([`band_foam_scales`]).
pub const BAND_SHARE_MIN: f32 = 0.05;

/// What one step of the history does: the growth factor `g` and the decay per step `d`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FoamRates {
    /// Share of the gap to the crest's foam closed this step, 0..=1.
    pub growth: f32,
    /// Foam lost this step where the crest has less, 0..=1.
    pub decay: f32,
}

impl FoamRates {
    /// The rates of `ocean` over a step of `dt` seconds (0: a restart, which grows nothing unless
    /// growth is instant).
    #[must_use]
    pub fn of(ocean: &FftOcean, dt: f64) -> Self {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let growth = if ocean.foam_growth > 0.0 {
            1.0 - (-dt / f64::from(ocean.foam_growth)).exp()
        } else {
            1.0
        };
        let decay = if ocean.foam_decay > 0.0 {
            dt / f64::from(ocean.foam_decay)
        } else {
            1.0
        };
        Self {
            growth: growth.clamp(0.0, 1.0) as f32,
            decay: decay.clamp(0.0, 1.0) as f32,
        }
    }
}

/// One step of one history texel: growth toward the crest's foam `inject`, or linear decay that
/// never goes below it (mirrors `water_ocean.wgsl` `foam_step`).
#[must_use]
pub fn foam_step(previous: f32, inject: f32, rates: FoamRates) -> f32 {
    if inject >= previous {
        previous + (inject - previous) * rates.growth
    } else {
        (previous - rates.decay).max(inject)
    }
}

/// The step from the history's last instant `last` to `now` (s): `None` restarts the history (no
/// last instant, a step back or longer than [`FOAM_MAX_STEP`]).
#[must_use]
pub fn foam_interval(last: Option<f64>, now: f64) -> Option<f64> {
    let dt = now - last?;
    (dt.is_finite() && (0.0..=FOAM_MAX_STEP).contains(&dt)).then_some(dt)
}

/// Each band's normalization of its own compression: one over its share of the deviation of
/// `1 − J`, `sqrt(σ_b² / Σ σ²)`, with `σ²` the band's slope variance, the share at least
/// [`BAND_SHARE_MIN`]. 1 for every band of a flat sea.
#[must_use]
pub fn band_foam_scales(tables: &OceanTables) -> [f32; OCEAN_BANDS] {
    let total: f64 = tables.bands.iter().map(|b| b.slope_variance).sum();
    tables.bands.each_ref().map(|band| {
        if total > 0.0 {
            let share = ((band.slope_variance / total).sqrt() as f32).max(BAND_SHARE_MIN);
            1.0 / share
        } else {
            1.0
        }
    })
}

/// The crest one band alone makes, 0..=1, from its Jacobian `jacobian`, its normalization `scale`
/// ([`band_foam_scales`]) and the ocean's onset (mirrors `water_ocean.wgsl` `cs_foam`).
#[must_use]
pub fn band_injection(jacobian: f32, scale: f32, onset: f32) -> f32 {
    let x = (((1.0 - jacobian) * scale - onset) / FOAM_RAMP).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// The Jacobian of one band's texel `i`, as `water_ocean.wgsl` forms it.
fn grid_jacobian(grid: &OceanGrid, i: usize) -> f32 {
    let at = |v: &[f32]| v.get(i).copied().unwrap_or(0.0);
    let (xx, zz, xz) = (at(&grid.dxx), at(&grid.dzz), at(&grid.dxz));
    (1.0 + xx) * (1.0 + zz) - xz * xz
}

/// The CPU mirror of the lingering-foam history: one map per band on the band's tile.
#[derive(Debug, Clone, PartialEq)]
pub struct FoamHistory {
    /// The ocean it belongs to.
    pub ocean: FftOcean,
    /// Texels per side.
    pub size: u32,
    /// Per band, `size²` values 0..=1, row along z.
    pub bands: [Vec<f32>; OCEAN_BANDS],
    /// The instant of the last step (s); `None` before the first and after a reset.
    pub seconds: Option<f64>,
    /// Steps taken.
    pub steps: u64,
    /// Steps that restarted the history.
    pub restarts: u64,
}

impl FoamHistory {
    /// An empty history of `ocean` on grids of `size` texels.
    #[must_use]
    pub fn new(ocean: &FftOcean, size: u32) -> Self {
        let texels = (size * size) as usize;
        Self {
            ocean: *ocean,
            size,
            bands: [0, 1, 2].map(|_| vec![0.0; texels]),
            seconds: None,
            steps: 0,
            restarts: 0,
        }
    }

    /// Forgets the past: the next step starts from no foam (a camera cut, a body change).
    pub fn reset(&mut self) {
        self.seconds = None;
    }

    /// Steps the history to `frame`, the ocean's maps at `seconds` (same size). Returns whether
    /// the step restarted it ([`foam_interval`]).
    pub fn step(&mut self, frame: &OceanFrame, seconds: f64) -> bool {
        let _span = tracing::info_span!("water_ocean_foam_mirror").entered();
        let dt = foam_interval(self.seconds, seconds);
        let restart = dt.is_none();
        let rates = FoamRates::of(&self.ocean, dt.unwrap_or(0.0));
        let scales = band_foam_scales(&frame.tables);
        let onset = self.ocean.foam_onset;
        for ((history, grid), scale) in self.bands.iter_mut().zip(&frame.grids).zip(scales) {
            for (i, h) in history.iter_mut().enumerate() {
                let inject = band_injection(grid_jacobian(grid, i), scale, onset);
                let previous = if restart { 0.0 } else { *h };
                *h = foam_step(previous, inject, rates);
            }
        }
        self.seconds = Some(seconds);
        self.steps += 1;
        self.restarts += u64::from(restart);
        restart
    }

    /// The value of `band` at the texel `(column, row)`.
    #[must_use]
    pub fn texel(&self, band: usize, column: u32, row: u32) -> f32 {
        let n = self.size as usize;
        self.bands
            .get(band)
            .and_then(|b| b.get(row as usize * n + column as usize))
            .copied()
            .unwrap_or(0.0)
    }

    /// `band`'s history at the rest point `(x, z)` on its tile `tile` (m), bilinear and wrapping
    /// as the fragment shader reads it.
    #[must_use]
    pub fn sample(&self, band: usize, tile: f64, x: f32, z: f32) -> f32 {
        let Some(values) = self.bands.get(band) else {
            return 0.0;
        };
        BilinearCell::new(f64::from(x), f64::from(z), tile, self.size).of(values)
    }
}

/// The drawn Jacobian `jacobian` scaled back up by the deviation the faded bands lost: `total`
/// is the bands' summed slope variance, `lost` what fading took (mirrors `water_body.wgsl`
/// `ocean_foam_extra`).
#[must_use]
pub fn rescaled_jacobian(jacobian: f32, total: f32, lost: f32) -> f32 {
    if total <= 0.0 {
        return jacobian;
    }
    let drawn = (total - lost).max(0.0);
    let r = if drawn > 0.0 {
        (total / drawn).sqrt().min(WHITECAP_RESCALE_MAX)
    } else {
        WHITECAP_RESCALE_MAX
    };
    1.0 - r * (1.0 - jacobian)
}

impl OceanFrame {
    /// The ocean's crest foam as the fragment shader draws it at the rest point `(x, z)` on
    /// `footprint` meters per pixel: each band faded on the footprint, the Jacobian of the sum,
    /// rescaled when the ocean asks ([`FftOcean::whitecap_rescale`]), and the lingering foam of
    /// `history` (mirrors `water_body.wgsl` `ocean_frame` and `ocean_foam_extra`).
    #[must_use]
    pub fn drawn_foam(&self, x: f32, z: f32, footprint: f32, history: Option<&FoamHistory>) -> f32 {
        let ocean = &self.tables.ocean;
        let (mut dxx, mut dzz, mut dxz) = (0.0f32, 0.0f32, 0.0f32);
        let (mut total, mut lost, mut lingering) = (0.0f32, 0.0f32, 0.0f32);
        for (b, (band, grid)) in self.tables.bands.iter().zip(&self.grids).enumerate() {
            let fade = crate::water::vertex_wave_fade(footprint, band.rms_k as f32);
            let variance = band.slope_variance as f32;
            total += variance;
            lost += (1.0 - fade * fade) * variance;
            if fade > 0.0 {
                let c = BilinearCell::new(f64::from(x), f64::from(z), band.tile, self.tables.size);
                dxx += fade * c.of(&grid.dxx);
                dzz += fade * c.of(&grid.dzz);
                dxz += fade * c.of(&grid.dxz);
                if let Some(h) = history {
                    lingering = lingering.max(fade * h.sample(b, band.tile, x, z));
                }
            }
        }
        let mut jacobian = (1.0 + dxx) * (1.0 + dzz) - dxz * dxz;
        if ocean.whitecap_rescale {
            jacobian = rescaled_jacobian(jacobian, total, lost);
        }
        let instant = crest_foam(jacobian, ocean.crest_foam, ocean.foam_onset);
        instant.max((ocean.crest_foam * lingering).clamp(0.0, 1.0))
    }
}

/// The mean drawn crest foam of `frame` on `footprint` meters per pixel: the share of the surface
/// a far view sees as whitecaps, over the longest tile on a `samples`² lattice (no history).
#[must_use]
pub fn whitecap_coverage(frame: &OceanFrame, footprint: f32, samples: u32) -> f32 {
    let n = samples.max(1);
    let step = frame.tables.bands[0].tile as f32 / n as f32;
    let sum: f64 = (0..n * n)
        .map(|i| {
            let (x, z) = ((i % n) as f32 * step + 0.37, (i / n) as f32 * step + 0.61);
            f64::from(frame.drawn_foam(x, z, footprint, None))
        })
        .sum();
    (sum / f64::from(n * n)) as f32
}

#[cfg(test)]
mod tests {
    use super::super::ocean::{OCEAN_QUERY_SIZE, frame_at, loop_fraction, tables_for};
    use super::*;

    fn lingering() -> FftOcean {
        FftOcean {
            wind_speed: 16.0,
            foam_decay: 2.0,
            foam_growth: 0.25,
            ..FftOcean::default()
        }
    }

    /// One texel on fixed steps: a crest held for half a second, then gone. The foam persists
    /// for exactly `ceil(H / d)` steps after the crest has gone, falls at every one of them and
    /// ends at zero.
    #[test]
    fn foam_persists_after_the_crest_and_decays_monotonically_to_zero() {
        let ocean = lingering();
        let rates = FoamRates::of(&ocean, 1.0 / 60.0);
        let mut h = 0.0f32;
        for _ in 0..30 {
            let next = foam_step(h, 1.0, rates);
            assert!(next >= h, "grows while the crest is on");
            h = next;
        }
        // Half a second of crest at a 0.25 s growth: 1 − e^(−2) of full foam.
        assert!((h - (1.0 - (-2.0f32).exp())).abs() < 1.0e-3, "{h}");
        let expected = (h / rates.decay).ceil() as u32;
        let mut steps = 0u32;
        while h > 0.0 {
            let next = foam_step(h, 0.0, rates);
            assert!(next < h, "falls at every step: {h} -> {next}");
            h = next;
            steps += 1;
            assert!(steps <= expected, "still {h} after {steps} steps");
        }
        assert_eq!(steps, expected);
        // At a 2 s decay that is most of 1.7 seconds of lingering foam at 60 steps a second.
        assert!(steps >= 100, "{steps}");
        assert_eq!(foam_step(0.0, 0.0, rates), 0.0, "stays at zero");
    }

    /// Instant growth takes the crest's foam at once; a crest smaller than the history neither
    /// raises nor cuts it below the crest.
    #[test]
    fn growth_and_decay_bounds() {
        let instant = FoamRates::of(
            &FftOcean {
                foam_growth: 0.0,
                ..lingering()
            },
            1.0 / 60.0,
        );
        assert_eq!(instant.growth, 1.0);
        assert_eq!(foam_step(0.2, 0.7, instant), 0.7);
        let rates = FoamRates::of(&lingering(), 1.0 / 60.0);
        assert_eq!(foam_step(0.8, 0.795, rates), 0.795, "decay stops at the crest");
        assert!((foam_step(0.8, 0.1, rates) - (0.8 - rates.decay)).abs() < 1.0e-7);
        let restart = FoamRates::of(&lingering(), 0.0);
        assert_eq!(restart.growth, 0.0);
        assert_eq!(restart.decay, 0.0);
    }

    /// The history restarts without a last instant, on a step back and on a long jump.
    #[test]
    fn steps_back_and_long_jumps_restart_the_history() {
        assert_eq!(foam_interval(None, 10.0), None);
        assert_eq!(foam_interval(Some(10.0), 10.5), Some(0.5));
        assert_eq!(foam_interval(Some(10.0), 10.0), Some(0.0));
        assert_eq!(foam_interval(Some(10.0), 9.9), None);
        assert_eq!(foam_interval(Some(10.0), 10.0 + FOAM_MAX_STEP + 0.01), None);
        assert_eq!(foam_interval(Some(10.0), f64::NAN), None);
    }

    /// On a real sea, on fixed steps: every texel whose crest has gone loses its foam at every step
    /// while the crest stays away; after three seconds the foam left where the Jacobian has
    /// already recovered covers at least a fifth as many texels as the crests themselves (instant
    /// foam has none there, so this fails without a history; measured 1974 behind 7035 crest
    /// texels, 0.28); and a reset forgets it.
    #[test]
    fn on_a_real_sea_foam_lingers_behind_the_crests() {
        let ocean = lingering();
        let tables = tables_for(&ocean, OCEAN_QUERY_SIZE);
        let scales = band_foam_scales(&tables);
        let mut history = FoamHistory::new(&ocean, OCEAN_QUERY_SIZE);
        let step = 1.0 / 30.0;
        let mut previous: Option<(OceanFrame, FoamHistory)> = None;
        for tick in 0..90u32 {
            let seconds = 300.0 + f64::from(tick) * step;
            let frame = OceanFrame::at(&tables, loop_fraction(seconds));
            let restarted = history.step(&frame, seconds);
            assert_eq!(restarted, tick == 0);
            if let Some((_, old)) = &previous {
                for (b, (now_band, was_band)) in history.bands.iter().zip(&old.bands).enumerate() {
                    for (i, (&h, &was)) in now_band.iter().zip(was_band).enumerate() {
                        let now =
                            band_injection(grid_jacobian(&frame.grids[b], i), scales[b], ocean.foam_onset);
                        assert!((0.0..=1.0).contains(&h), "{h}");
                        if now == 0.0 && was > 0.0 {
                            assert!(h < was, "falls once the crest has gone");
                            assert!(h >= 0.0);
                        }
                    }
                }
            }
            previous = Some((frame, history.clone()));
        }
        let (mut crests, mut lingering) = (0u32, 0u32);
        if let Some((last, _)) = &previous {
            for (b, band) in history.bands.iter().enumerate() {
                for (i, &h) in band.iter().enumerate() {
                    let now = band_injection(grid_jacobian(&last.grids[b], i), scales[b], ocean.foam_onset);
                    crests += u32::from(now > 0.0);
                    lingering += u32::from(now == 0.0 && h > 0.0);
                }
            }
        }
        eprintln!("after 3 s: {crests} crest texels, {lingering} texels of foam behind them");
        assert!(crests > 100, "crests: {crests}");
        assert!(
            5 * lingering >= crests,
            "foam behind the crests: {lingering} of {crests}"
        );
        let sum: f32 = history.bands.iter().flatten().sum();
        assert!(sum > 0.0, "the sea has lingering foam");
        history.reset();
        let frame = OceanFrame::at(&tables, loop_fraction(500.0));
        assert!(history.step(&frame, 500.0), "a reset restarts");
        let fresh: f32 = history.bands.iter().flatten().sum();
        assert!(fresh < 1.0e-6, "growth from nothing at Δt 0: {fresh}");
    }

    /// Each band's normalized compression has about the deviation of the summed surface's, so no
    /// band is left out of the history, whatever its share of the slopes.
    #[test]
    fn band_scales_even_out_the_bands() {
        let ocean = lingering();
        let frame = frame_at(&ocean, 120.0);
        let scales = band_foam_scales(&frame.tables);
        let deviation = |values: &mut dyn Iterator<Item = f32>| {
            let v: Vec<f64> = values.map(f64::from).collect();
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
        };
        let n = OCEAN_QUERY_SIZE as usize;
        let tile = frame.tables.bands[0].tile as f32;
        let sum = deviation(&mut (0..n * n).map(|i| {
            let (x, z) = ((i % n) as f32 * tile / n as f32, (i / n) as f32 * tile / n as f32);
            1.0 - frame.sample(x, z).jacobian
        }));
        for (b, grid) in frame.grids.iter().enumerate() {
            let band = deviation(&mut (0..n * n).map(|i| (1.0 - grid_jacobian(grid, i)) * scales[b]));
            eprintln!(
                "band {b}: scale {}, normalized deviation {band} against the sum's {sum}",
                scales[b]
            );
            assert!(scales[b] >= 1.0, "{scales:?}");
            // A band capped at the smallest share stays below the sum's deviation by design.
            let capped = scales[b] >= 1.0 / BAND_SHARE_MIN;
            assert!(band < 1.6 * sum, "band {b}: {band} against {sum}");
            assert!(capped || band > 0.6 * sum, "band {b}: {band} against {sum}");
        }
    }

    /// A default ocean draws exactly its instantaneous foam: no history, no rescale.
    #[test]
    fn default_ocean_draws_the_instant_foam() {
        let ocean = FftOcean {
            wind_speed: 16.0,
            ..FftOcean::default()
        };
        assert_eq!(ocean.foam_decay, 0.0);
        assert!(!ocean.whitecap_rescale);
        let frame = frame_at(&ocean, 77.0);
        for i in 0..64 {
            let (x, z) = (i as f32 * 13.7, i as f32 * -5.3);
            let j = frame.sample(x, z).jacobian;
            let want = crest_foam(j, ocean.crest_foam, ocean.foam_onset);
            assert!((frame.drawn_foam(x, z, 0.0, None) - want).abs() < 1.0e-5);
        }
    }

    /// The rescale gives back the deviation the faded bands took, capped.
    #[test]
    fn rescale_restores_the_lost_deviation() {
        assert_eq!(rescaled_jacobian(0.9, 0.0, 0.0), 0.9);
        assert_eq!(rescaled_jacobian(0.9, 2.0, 0.0), 0.9);
        assert!((rescaled_jacobian(0.9, 2.0, 1.5) - 0.8).abs() < 1.0e-6);
        assert!((rescaled_jacobian(0.9, 2.0, 2.0) - (1.0 - 0.1 * WHITECAP_RESCALE_MAX)).abs() < 1.0e-6);
        assert_eq!(rescaled_jacobian(1.0, 2.0, 1.9), 1.0, "flat water stays flat");
    }

    /// Whitecaps with distance on a fixed sea: without the rescale the drawn coverage falls as the
    /// bands fade on the footprint (to 4 % of the near coverage at 8 m pixels); with it, coverage
    /// stays between two thirds and one and a half times the near coverage out to 8 m pixels
    /// (about 7.5 km at 1080 rows and a 60° view).
    #[test]
    fn whitecap_coverage_holds_with_distance_when_rescaled() {
        let sea = FftOcean {
            wind_speed: 14.0,
            ..FftOcean::default()
        };
        let rescaled = FftOcean {
            whitecap_rescale: true,
            ..sea
        };
        let plain = frame_at(&sea, 90.0);
        let kept = frame_at(&rescaled, 90.0);
        let near = whitecap_coverage(&plain, 0.0, 192);
        assert!(near > 0.003, "{near}");
        assert!(
            (whitecap_coverage(&kept, 0.0, 192) - near).abs() < 1.0e-6,
            "nothing to rescale near"
        );
        let mut report = Vec::new();
        let mut thinnest_plain = f32::INFINITY;
        for footprint in [0.5f32, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0] {
            let p = whitecap_coverage(&plain, footprint, 192);
            let k = whitecap_coverage(&kept, footprint, 192);
            report.push((footprint, p / near, k / near));
            thinnest_plain = thinnest_plain.min(p / near);
        }
        eprintln!("footprint, plain / near, rescaled / near: {report:?}");
        assert!(thinnest_plain < 0.5, "the problem is real: {report:?}");
        for (footprint, _, k) in &report {
            assert!((0.67..=1.5).contains(k), "{footprint} m: {report:?}");
        }
    }
}
