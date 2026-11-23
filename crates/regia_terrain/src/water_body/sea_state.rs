// SPDX-License-Identifier: MPL-2.0
//! The sea-state table (spec 100 § 3, ADR-2342): wind speed in, a plane body's [`Swell`] out.
//!
//! The rows follow the fully developed sea of the Pierson–Moskowitz spectrum (Pierson and
//! Moskowitz 1964, "A proposed spectral form for fully developed wind seas": significant height
//! `Hs ≈ 0.0213 U²`, peak wavelength `λp ≈ 0.83 U²` for the wind `U` at 10 m), one row per
//! Beaufort force (WMO sea-state descriptions for the crest-foam columns). Between two rows the
//! values interpolate linearly in the wind speed; above the last row they hold. A limited fetch
//! (`fetch` below 1) lowers the height by `fetch` and the wavelength by `√fetch`, which is this
//! engine's own simplification of the fetch-limited growth curves (JONSWAP, Hasselmann et al.
//! 1973), not a fit to them.
//!
//! [`SeaState::swell`] builds the waves with [`Swell::open_sea`], so the result is deterministic
//! and valid ([`Swell::validate`]). Quantizing the wind ([`quantize_wind`]) before the lookup keeps
//! a changing weather from rebuilding the swell every frame.

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use super::WaterBody;
use super::ocean::FftOcean;
use super::swell::{MAX_SWELL_WAVES, Swell};

/// One row of [`SEA_TABLE`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeaRow {
    /// Beaufort force the row is named for.
    pub force: u8,
    /// Wind speed at 10 m (m/s).
    pub wind_m_per_s: f32,
    /// Significant wave height (m).
    pub height_m: f32,
    /// Peak wavelength (m).
    pub wavelength_m: f32,
    /// Crest foam strength of the [`Swell`].
    pub crest_foam: f32,
    /// Foam onset (`1 − J`) of the [`Swell`]; lower foams more.
    pub foam_onset: f32,
}

macro_rules! row {
    ($f:expr, $u:expr, $h:expr, $l:expr, $c:expr, $o:expr) => {
        SeaRow {
            force: $f,
            wind_m_per_s: $u,
            height_m: $h,
            wavelength_m: $l,
            crest_foam: $c,
            foam_onset: $o,
        }
    };
}

/// The table, by rising wind speed.
pub const SEA_TABLE: [SeaRow; 13] = [
    row!(0, 0.0, 0.0, 0.25, 0.0, 0.30),
    row!(1, 1.0, 0.02, 0.83, 0.2, 0.28),
    row!(2, 2.5, 0.13, 5.2, 0.4, 0.26),
    row!(3, 4.5, 0.43, 16.8, 0.6, 0.22),
    row!(4, 7.0, 1.04, 40.6, 0.9, 0.18),
    row!(5, 9.5, 1.92, 74.8, 1.2, 0.15),
    row!(6, 12.5, 3.33, 129.5, 1.5, 0.13),
    row!(7, 15.5, 5.12, 199.0, 1.8, 0.11),
    row!(8, 19.0, 7.69, 299.0, 2.1, 0.10),
    row!(9, 22.5, 10.8, 420.0, 2.4, 0.09),
    row!(10, 26.5, 14.96, 582.0, 2.7, 0.08),
    row!(11, 30.5, 19.8, 771.0, 3.0, 0.07),
    row!(12, 35.0, 26.1, 1015.0, 3.3, 0.06),
];

/// Wind speeds are rounded to this step (m/s) by [`quantize_wind`].
pub const WIND_STEP: f32 = 0.5;

/// Smallest fetch factor.
pub const MIN_FETCH: f32 = 0.1;

/// The wind rounded to [`WIND_STEP`] (non-finite and negative speeds are 0).
#[must_use]
pub fn quantize_wind(speed: f32) -> f32 {
    if speed.is_finite() && speed > 0.0 {
        (speed / WIND_STEP).round() * WIND_STEP
    } else {
        0.0
    }
}

/// A sea state: the wind and how developed the sea is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeaState {
    /// Wind speed at 10 m (m/s).
    pub wind_m_per_s: f32,
    /// Direction the wind blows toward on world X and Z (any non-zero length; `+x` for zero).
    pub direction: [f32; 2],
    /// How developed the sea is, [`MIN_FETCH`]`..=1` (1 fully developed).
    pub fetch: f32,
}

impl SeaState {
    /// A fully developed sea in `wind` (m/s on world X and Z; the waves travel with it).
    #[must_use]
    pub fn from_wind(wind: [f32; 2]) -> Self {
        let speed = (wind[0] * wind[0] + wind[1] * wind[1]).sqrt();
        Self {
            wind_m_per_s: speed,
            direction: if speed > 1.0e-4 { wind } else { [1.0, 0.0] },
            fetch: 1.0,
        }
    }

    /// The interpolated row for this wind speed (the fetch is not applied).
    #[must_use]
    pub fn row(&self) -> SeaRow {
        let u = if self.wind_m_per_s.is_finite() {
            self.wind_m_per_s.max(0.0)
        } else {
            0.0
        };
        let last = SEA_TABLE[SEA_TABLE.len() - 1];
        if u >= last.wind_m_per_s {
            return SeaRow {
                wind_m_per_s: u,
                ..last
            };
        }
        let i = SEA_TABLE.partition_point(|r| r.wind_m_per_s <= u).max(1);
        let (a, b) = (SEA_TABLE[i - 1], SEA_TABLE[i]);
        let t = (u - a.wind_m_per_s) / (b.wind_m_per_s - a.wind_m_per_s);
        let mix = |x: f32, y: f32| x + (y - x) * t;
        SeaRow {
            force: if t < 0.5 { a.force } else { b.force },
            wind_m_per_s: u,
            height_m: mix(a.height_m, b.height_m),
            wavelength_m: mix(a.wavelength_m, b.wavelength_m),
            crest_foam: mix(a.crest_foam, b.crest_foam),
            foam_onset: mix(a.foam_onset, b.foam_onset),
        }
    }

    /// Significant height (m) and peak wavelength (m) after the fetch.
    #[must_use]
    pub fn height_and_wavelength(&self) -> (f32, f32) {
        let fetch = if self.fetch.is_finite() {
            self.fetch.clamp(MIN_FETCH, 1.0)
        } else {
            1.0
        };
        let row = self.row();
        (row.height_m * fetch, (row.wavelength_m * fetch.sqrt()).max(0.25))
    }

    /// The swell of this sea with `count` waves (1..=[`MAX_SWELL_WAVES`]); `None` when the sea is
    /// flat (no wind, or a height under 1 mm).
    #[must_use]
    pub fn swell(&self, count: usize) -> Option<Swell> {
        let (height, wavelength) = self.height_and_wavelength();
        if height < 1.0e-3 {
            return None;
        }
        let mut swell = Swell::open_sea(
            self.direction,
            height,
            wavelength,
            count.clamp(1, MAX_SWELL_WAVES),
        );
        let row = self.row();
        swell.crest_foam = row.crest_foam;
        swell.foam_onset = row.foam_onset;
        Some(swell)
    }
}

/// The wind the sea follows (m/s on world X and Z, at 10 m): written by the host or by a bridge
/// from the weather (`regia_framework::sea_weather::SeaWeatherPlugin`), read by
/// [`apply_weather_sea`]. Calm by default.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct SeaWind {
    /// Wind speed on world X and Z (m/s).
    pub wind_m_per_s: [f32; 2],
}

fn default_fetch() -> f32 {
    1.0
}

fn default_waves() -> usize {
    12
}

/// Puts a [`WaterBody`] plane's sea under the wind: [`apply_weather_sea`] rewrites the body's
/// `swell` from [`SeaWind`] each time the (quantized) wind changes, or, on a body with an FFT
/// ocean, the ocean's wind ([`WeatherSea::ocean_for`]). The component owns what it writes: a swell
/// (or ocean wind) set by hand on the same body is replaced. Opt-in: bodies without it never
/// change.
#[derive(Component, Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WeatherSea {
    /// How developed the sea is, [`MIN_FETCH`]`..=1` ([`SeaState::fetch`]).
    pub fetch: f32,
    /// Waves of the swell, `1..=`[`MAX_SWELL_WAVES`].
    pub waves: usize,
}

impl Default for WeatherSea {
    fn default() -> Self {
        Self {
            fetch: default_fetch(),
            waves: default_waves(),
        }
    }
}

impl WeatherSea {
    /// The sea state this component asks for under `wind`, the speed quantized ([`quantize_wind`]).
    #[must_use]
    pub fn state_for(&self, wind: SeaWind) -> SeaState {
        let [x, z] = wind.wind_m_per_s;
        let speed = quantize_wind((x * x + z * z).sqrt());
        let mut sea = SeaState::from_wind(wind.wind_m_per_s);
        sea.wind_m_per_s = speed;
        sea.fetch = self.fetch;
        sea
    }
}

/// Ocean wind speeds are rounded to this step (m/s) by [`WeatherSea::ocean_for`]: a new wind
/// rebuilds the ocean's spectrum, so the step is coarser than the swell's [`WIND_STEP`].
pub const OCEAN_WIND_STEP: f32 = 1.0;

/// Ocean wind headings are rounded to this step (degrees) by [`WeatherSea::ocean_for`].
pub const OCEAN_HEADING_STEP_DEG: f32 = 5.0;

impl WeatherSea {
    /// `ocean` with the wind of `wind` (ADR-2342): the speed rounded to [`OCEAN_WIND_STEP`] and
    /// clamped to the ocean's 0.5..=40 m/s, the heading rounded to [`OCEAN_HEADING_STEP_DEG`]. A
    /// calm or non-finite wind keeps the ocean's heading. Only the wind changes: the seed, fetch
    /// and look stay the author's.
    #[must_use]
    pub fn ocean_for(&self, ocean: &FftOcean, wind: SeaWind) -> FftOcean {
        let [x, z] = wind.wind_m_per_s;
        let speed = (x * x + z * z).sqrt();
        let mut next = *ocean;
        let quantized = if speed.is_finite() {
            (speed / OCEAN_WIND_STEP).round() * OCEAN_WIND_STEP
        } else {
            0.0
        };
        next.wind_speed = quantized.clamp(0.5, 40.0);
        if speed.is_finite() && speed > 1.0e-3 {
            let step = OCEAN_HEADING_STEP_DEG.to_radians();
            let heading = (z.atan2(x) / step).round() * step;
            let (sin, cos) = heading.sin_cos();
            // Exact axes stay exact (a 90 degree heading has cos = -4e-8 in f32).
            let snap = |v: f32| if v.abs() < 1.0e-6 { 0.0 } else { v };
            next.wind_direction = [snap(cos), snap(sin)];
        }
        next
    }
}

/// Rewrites every plane [`WaterBody`] with a [`WeatherSea`] from [`SeaWind`] (span
/// `water_sea_state`): a body with an FFT ocean gets the wind on its ocean
/// ([`WeatherSea::ocean_for`]) and keeps its swell; any other plane gets the sea-state table's
/// swell. A body is written only when it would change, so a steady wind costs one comparison per
/// body and no change detection.
pub fn apply_weather_sea(wind: Res<SeaWind>, mut bodies: Query<(&WeatherSea, &mut WaterBody)>) {
    let _span = tracing::debug_span!("water_sea_state").entered();
    for (sea, mut body) in &mut bodies {
        if !matches!(body.shape, super::WaterShape::Plane { .. }) {
            continue;
        }
        if let Some(ocean) = body.ocean {
            let next = sea.ocean_for(&ocean, *wind);
            if next != ocean {
                body.ocean = Some(next);
            }
            continue;
        }
        let next = sea.state_for(*wind).swell(sea.waves);
        if body.swell != next {
            body.swell = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_rises_and_matches_the_spectrum_fit() {
        for pair in SEA_TABLE.windows(2) {
            assert!(pair[0].wind_m_per_s < pair[1].wind_m_per_s);
            assert!(pair[0].height_m <= pair[1].height_m);
            assert!(pair[0].wavelength_m <= pair[1].wavelength_m);
            assert!(pair[0].crest_foam <= pair[1].crest_foam);
            assert!(pair[0].foam_onset >= pair[1].foam_onset);
        }
        for r in &SEA_TABLE[2..] {
            let u2 = r.wind_m_per_s * r.wind_m_per_s;
            assert!((r.height_m / (0.0213 * u2) - 1.0).abs() < 0.04, "{r:?}");
            assert!((r.wavelength_m / (0.83 * u2) - 1.0).abs() < 0.03, "{r:?}");
        }
    }

    #[test]
    fn rows_interpolate_and_clamp() {
        let at = |u: f32| SeaState::from_wind([u, 0.0]).row();
        assert_eq!(at(7.0).height_m, 1.04);
        let mid = at(8.25);
        assert!((mid.height_m - 0.5 * (1.04 + 1.92)).abs() < 1.0e-5);
        assert_eq!(at(100.0).height_m, 26.1);
        assert_eq!(at(0.0).height_m, 0.0);
        assert_eq!(
            SeaState {
                wind_m_per_s: f32::NAN,
                ..SeaState::from_wind([0.0, 0.0])
            }
            .row()
            .height_m,
            0.0
        );
    }

    #[test]
    fn swell_is_valid_follows_the_wind_and_rises_with_it() {
        let mut last = 0.0;
        for u in [2.0, 5.0, 9.0, 14.0, 20.0, 30.0, 40.0] {
            let sea = SeaState::from_wind([0.0, u]);
            let swell = sea.swell(8).expect("wind makes waves");
            swell.validate().expect("valid");
            let rss = swell
                .waves
                .iter()
                .map(|w| w.height * w.height)
                .sum::<f32>()
                .sqrt();
            assert!(rss >= last, "{u}: {rss}");
            last = rss;
            // The longest wave travels downwind.
            assert!(swell.waves[0].unit_direction()[1] > 0.9, "{u}");
        }
        assert!(SeaState::from_wind([0.0, 0.0]).swell(8).is_none());
    }

    #[test]
    fn fetch_lowers_the_sea() {
        let full = SeaState::from_wind([10.0, 0.0]);
        let short = SeaState { fetch: 0.25, ..full };
        let (hf, lf) = full.height_and_wavelength();
        let (hs, ls) = short.height_and_wavelength();
        assert!((hs / hf - 0.25).abs() < 1.0e-5 && (ls / lf - 0.5).abs() < 1.0e-5);
        let clamped = SeaState { fetch: -3.0, ..full };
        assert!((clamped.height_and_wavelength().0 / hf - MIN_FETCH).abs() < 1.0e-5);
    }

    /// Spec 100 § 3: the fetch is held to `MIN_FETCH..=1` (a fetch above 1 is a full sea, a
    /// non-finite one too), and a sea under 1 mm high has no swell, whether the wind or the fetch
    /// makes it that small.
    #[test]
    fn fetch_is_held_to_its_range_and_a_millimetre_sea_is_flat() {
        let full = SeaState::from_wind([10.0, 0.0]);
        let want = full.height_and_wavelength();
        for fetch in [1.0, 1.5, 40.0, f32::INFINITY, f32::NAN] {
            assert_eq!(
                SeaState { fetch, ..full }.height_and_wavelength(),
                want,
                "{fetch}"
            );
        }
        // Force 1 is 0.02 m at 1 m/s: 0.1 m/s interpolates to 2 mm, 0.04 m/s to 0.8 mm.
        let breeze = SeaState::from_wind([0.1, 0.0]);
        assert!(breeze.height_and_wavelength().0 >= 1.0e-3);
        assert!(breeze.swell(4).is_some());
        assert!(SeaState::from_wind([0.04, 0.0]).swell(4).is_none());
        let short = SeaState {
            fetch: MIN_FETCH,
            ..breeze
        };
        assert!(short.height_and_wavelength().0 < 1.0e-3);
        assert!(short.swell(4).is_none(), "the fetch made it flat");
        // The wave count is held to 1..=MAX_SWELL_WAVES.
        let storm = SeaState::from_wind([20.0, 0.0]);
        assert_eq!(storm.swell(0).expect("storm").waves.len(), 1);
        assert_eq!(storm.swell(1000).expect("storm").waves.len(), MAX_SWELL_WAVES);
    }

    /// Spec 100 § 3: "a steady wind writes nothing". After the first write, frames with the same
    /// (quantized) wind leave every body unchanged for change detection; a ribbon with the
    /// component is never written; an ocean body's own swell is not touched.
    #[test]
    fn a_steady_wind_triggers_no_change_detection_and_skips_ribbons_and_ocean_swells() {
        use super::super::{RibbonPoint, WaterBody, WaterLook};
        #[derive(Resource, Default)]
        struct Changes(usize);
        fn count(mut changes: ResMut<Changes>, q: Query<(), Changed<WaterBody>>) {
            changes.0 += q.iter().count();
        }
        let mut app = App::new();
        app.insert_resource(SeaWind {
            wind_m_per_s: [0.0, 9.0],
        })
        .init_resource::<Changes>()
        .add_systems(Update, (apply_weather_sea, count).chain());
        let lake = app
            .world_mut()
            .spawn((
                WaterBody::plane([0.0, 1.0, 0.0], [50.0, 50.0], WaterLook::lake()),
                WeatherSea::default(),
            ))
            .id();
        let mut sea = WaterBody::plane([0.0; 3], [100.0, 100.0], WaterLook::lake());
        let authored = SeaState::from_wind([4.0, 0.0]).swell(3);
        sea.swell = authored.clone();
        sea.ocean = Some(FftOcean::default());
        let ocean = app.world_mut().spawn((sea, WeatherSea::default())).id();
        let point = |x: f32| RibbonPoint {
            position: [x, 0.0, 0.0],
            width: 4.0,
            depth: 1.0,
            speed: Some(1.0),
        };
        let stream = app
            .world_mut()
            .spawn((
                WaterBody::ribbon(vec![point(0.0), point(20.0)], WaterLook::lake()),
                WeatherSea::default(),
            ))
            .id();
        app.update();
        // The spawn frame counts all three as added; the lake and the ocean were also written.
        assert!(app.world().get::<WaterBody>(lake).expect("lake").swell.is_some());
        let body = app.world().get::<WaterBody>(ocean).expect("ocean");
        assert_eq!(body.swell, authored, "the ocean body's swell is the author's");
        assert_eq!(body.ocean.expect("fft").wind_speed, 9.0);
        let ribbon = app.world().get::<WaterBody>(stream).expect("ribbon");
        assert!(ribbon.swell.is_none() && ribbon.ocean.is_none());
        app.world_mut().resource_mut::<Changes>().0 = 0;
        for _ in 0..5 {
            app.update();
        }
        // A wind inside the same steps (0.5 m/s for the swell, 1 m/s for the ocean) is steady too.
        app.world_mut().resource_mut::<SeaWind>().wind_m_per_s = [0.0, 9.1];
        app.update();
        assert_eq!(
            app.world().resource::<Changes>().0,
            0,
            "a steady wind wrote a body"
        );
        // A real change writes the lake and the ocean, never the ribbon.
        app.world_mut().resource_mut::<SeaWind>().wind_m_per_s = [20.0, 0.0];
        app.update();
        assert_eq!(app.world().resource::<Changes>().0, 2);
        let body = app.world().get::<WaterBody>(ocean).expect("ocean");
        assert_eq!(body.swell, authored);
        assert_eq!(body.ocean.expect("fft").wind_speed, 20.0);
    }

    #[test]
    fn quantized_wind_is_stable() {
        assert_eq!(quantize_wind(7.26), 7.5);
        assert_eq!(quantize_wind(7.24), 7.0);
        assert_eq!(quantize_wind(-1.0), 0.0);
        assert_eq!(quantize_wind(f32::NAN), 0.0);
        let a = SeaState::from_wind([quantize_wind(7.3), 0.0]).swell(6);
        let b = SeaState::from_wind([quantize_wind(7.4), 0.0]).swell(6);
        assert_eq!(a, b);
    }

    #[test]
    fn the_system_follows_the_wind_and_leaves_ribbons_and_steady_winds_alone() {
        use super::super::{WaterBody, WaterLook};
        let mut app = App::new();
        app.init_resource::<SeaWind>()
            .add_systems(Update, apply_weather_sea);
        let lake = app
            .world_mut()
            .spawn((
                WaterBody::plane([0.0, 1.0, 0.0], [50.0, 50.0], WaterLook::lake()),
                WeatherSea::default(),
            ))
            .id();
        let plain = app
            .world_mut()
            .spawn(WaterBody::plane([0.0, 1.0, 0.0], [50.0, 50.0], WaterLook::lake()))
            .id();
        app.update();
        assert!(
            app.world().get::<WaterBody>(lake).expect("lake").swell.is_none(),
            "calm"
        );
        app.world_mut().resource_mut::<SeaWind>().wind_m_per_s = [10.0, 0.0];
        app.update();
        let swell = app
            .world()
            .get::<WaterBody>(lake)
            .expect("lake")
            .swell
            .clone()
            .expect("swell");
        swell.validate().expect("valid");
        assert_eq!(swell.waves.len(), 12);
        assert!(
            app.world()
                .get::<WaterBody>(plain)
                .expect("plain")
                .swell
                .is_none()
        );
        // A wind change under the quantization step changes nothing.
        app.world_mut().resource_mut::<SeaWind>().wind_m_per_s = [10.1, 0.0];
        app.update();
        let after = app.world().get::<WaterBody>(lake).expect("lake");
        assert_eq!(after.swell.as_ref(), Some(&swell));
        // A storm raises the sea; the wave count follows the component.
        app.world_mut().resource_mut::<SeaWind>().wind_m_per_s = [18.0, 6.0];
        app.world_mut()
            .entity_mut(lake)
            .insert(WeatherSea { fetch: 0.5, waves: 4 });
        app.update();
        let storm = app
            .world()
            .get::<WaterBody>(lake)
            .expect("lake")
            .swell
            .clone()
            .expect("storm");
        assert_eq!(storm.waves.len(), 4);
        let rss = |s: &Swell| s.waves.iter().map(|w| w.height * w.height).sum::<f32>().sqrt();
        assert!(rss(&storm) > rss(&swell) * 0.9, "{} {}", rss(&storm), rss(&swell));
    }

    /// The ocean gets the wind quantized (1 m/s, 5 degrees) and clamped to its valid range; a calm
    /// wind keeps the heading; nothing else of the ocean changes; the result validates.
    #[test]
    fn ocean_wind_is_quantized_and_valid() {
        let sea = WeatherSea::default();
        let base = FftOcean {
            seed: 77,
            fetch: 12_000.0,
            wind_direction: [0.0, -1.0],
            ..FftOcean::default()
        };
        let o = sea.ocean_for(
            &base,
            SeaWind {
                wind_m_per_s: [7.3, 7.3],
            },
        );
        assert_eq!(o.wind_speed, 10.0);
        let heading = o.wind_direction[1].atan2(o.wind_direction[0]).to_degrees();
        assert!((heading - 45.0).abs() < 1.0e-3, "{heading}");
        assert_eq!((o.seed, o.fetch, o.tiles), (base.seed, base.fetch, base.tiles));
        assert!(o.validate().is_ok());
        // A small change of wind inside one step changes nothing.
        let p = sea.ocean_for(
            &base,
            SeaWind {
                wind_m_per_s: [7.4, 7.25],
            },
        );
        assert_eq!(o, p);
        // Calm: the floor speed, the authored heading.
        let calm = sea.ocean_for(&base, SeaWind::default());
        assert_eq!((calm.wind_speed, calm.wind_direction), (0.5, [0.0, -1.0]));
        assert!(calm.validate().is_ok());
        // A hurricane is held at the ocean's limit; non-finite input stays valid.
        let wild = sea.ocean_for(
            &base,
            SeaWind {
                wind_m_per_s: [90.0, 0.0],
            },
        );
        assert_eq!((wild.wind_speed, wild.wind_direction), (40.0, [1.0, 0.0]));
        let nan = sea.ocean_for(
            &base,
            SeaWind {
                wind_m_per_s: [f32::NAN, 1.0],
            },
        );
        assert!(nan.validate().is_ok());
        assert_eq!(nan.wind_direction, base.wind_direction);
    }

    /// The system writes an ocean body's wind and leaves its swell (none) alone.
    #[test]
    fn the_system_drives_an_ocean_body_wind() {
        let mut app = App::new();
        app.insert_resource(SeaWind {
            wind_m_per_s: [0.0, 21.6],
        })
        .add_systems(Update, apply_weather_sea);
        let mut body = WaterBody::plane([0.0; 3], [100.0, 100.0], super::super::WaterLook::lake());
        body.ocean = Some(FftOcean::default());
        let e = app.world_mut().spawn((body, WeatherSea::default())).id();
        app.update();
        let body = app.world().get::<WaterBody>(e).expect("body");
        let ocean = body.ocean.expect("ocean");
        assert_eq!(ocean.wind_speed, 22.0);
        assert_eq!(ocean.wind_direction, [0.0, 1.0]);
        assert!(body.swell.is_none());
    }
}
