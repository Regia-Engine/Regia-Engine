// SPDX-License-Identifier: MPL-2.0
//! Daylight calibration (ADR-1425): what a grey card lying on open ground shows under the
//! engine's sun and clear sky, checked against a published clear-sky daylight model, and where
//! the auto-exposure meter puts it.
//!
//! The engine's outdoor light is physical: the sun is the atmosphere's transmitted illuminance
//! (lux) and the sky the sky-view table's radiance (cd/m²), in the same units the HDR target
//! holds. A horizontal Lambertian card of albedo `ρ` therefore shows
//!
//! ```text
//! L_sun   = ρ / π · (E_sun · sin(el) + E_sky)      in the sun
//! L_shade = ρ / π · E_sky                           in a shadow that hides the sun only
//! ```
//!
//! where `E_sky = π · L̄_sky` is the cosine-weighted sky ([`crate::ambient_irradiance_table`],
//! the same term the ambient bake lights terrain and meshes with).
//!
//! **Reference.** The IES clear-sky daylight availability model (IES Calculation Procedures
//! Committee, "Recommended practice for the calculation of daylight availability", J. IES 13(4),
//! 1984): direct normal illuminance `E_dn = E_xt · exp(−c · m)` with the extraterrestrial
//! illuminance `E_xt = 127.5 klx`, the clear-sky extinction `c = 0.21` and the air mass
//! `m = 1 / sin(el)`, and the clear-sky diffuse horizontal illuminance
//! `E_kh = 0.8 + 15.5 · sin(el)^0.5` klx. At the showcase's 21° sun that is about 71 klx
//! direct normal (25 klx on the ground), 10 klx diffuse, and a 0.18 card at about 2,000 cd/m²
//! in the sun and 580 cd/m² in shade: shade is about a quarter of sun, never near black.
//!
//! **Exposure.** The auto-exposure meter averages log luminance over the frame between the
//! 40th and 95th percentile (`readback::average_log_luminance`) and maps that average to middle
//! grey 0.18 (`post::ev100_from_luminance`). [`metered_card`] runs that meter on a frame that is
//! part sky and part sunlit ground, so a test can say where the grey card lands.

use crate::AtmosphereLuts;

/// Extraterrestrial solar illuminance of the IES model (lux).
pub const IES_SOLAR_ILLUMINANCE_LUX: f32 = 127_500.0;
/// IES clear-sky atmospheric extinction coefficient (per air mass).
pub const IES_CLEAR_EXTINCTION: f32 = 0.21;
/// Albedo of a photographic grey card.
pub const GREY_CARD_ALBEDO: f32 = 0.18;

/// Rec. 709 luminance of a linear RGB triple.
#[must_use]
pub fn luminance(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// The IES clear-sky daylight model at sun elevation `elevation_rad`: (direct normal
/// illuminance, diffuse horizontal illuminance), both in lux. Zero for a sun at or below the
/// horizon.
#[must_use]
pub fn ies_clear_sky(elevation_rad: f32) -> (f32, f32) {
    let s = elevation_rad.sin();
    if !(s.is_finite() && s > 0.0) {
        return (0.0, 0.0);
    }
    let direct = IES_SOLAR_ILLUMINANCE_LUX * (-IES_CLEAR_EXTINCTION / s).exp();
    let diffuse = (0.8 + 15.5 * s.sqrt()) * 1000.0;
    (direct, diffuse)
}

/// A horizontal grey card under a sun and sky: the illuminances that reach it and the luminance
/// it shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GreyCard {
    /// Sun elevation (radians).
    pub elevation_rad: f32,
    /// Illuminance of the sun on a surface facing it (lux).
    pub direct_normal_lux: f32,
    /// Illuminance of the sun on the horizontal card (lux).
    pub direct_horizontal_lux: f32,
    /// Illuminance of the sky (without the sun) on the horizontal card (lux).
    pub diffuse_horizontal_lux: f32,
    /// Card albedo.
    pub albedo: f32,
    /// Luminance of the card in the sun (cd/m²).
    pub sunlit_cd_m2: f32,
    /// Luminance of the card in a shadow that hides the sun only (cd/m²).
    pub shade_cd_m2: f32,
}

impl GreyCard {
    /// The card under the IES clear-sky model at `elevation_rad`.
    #[must_use]
    pub fn ies(elevation_rad: f32, albedo: f32) -> Self {
        let (direct, diffuse) = ies_clear_sky(elevation_rad);
        Self::from_light(elevation_rad, direct, diffuse, albedo)
    }

    /// The card under the engine's atmosphere `luts` (sun and sky for the tables' sun
    /// direction): the transmitted sun of [`crate::transmitted_sun`] and the cosine-weighted
    /// sky of [`crate::ambient_irradiance_table`], the terms the renderer lights terrain with.
    #[must_use]
    pub fn engine(luts: &AtmosphereLuts, albedo: f32) -> Self {
        let elevation_rad = luts.sun_direction[1].clamp(-1.0, 1.0).asin();
        let (color, lux) = crate::transmitted_sun(luts, &luts.params);
        // `transmitted_sun` returns the mean-channel lux and a colour of mean 1 (ADR-1447):
        // colour × lux is the light the renderer puts on a surface, and its luminance the
        // photometric sun.
        let direct = lux * luminance(color);
        let diffuse = std::f32::consts::PI * luminance(crate::ambient_irradiance_table(luts));
        Self::from_light(elevation_rad, direct, diffuse, albedo)
    }

    fn from_light(
        elevation_rad: f32,
        direct_normal_lux: f32,
        diffuse_horizontal_lux: f32,
        albedo: f32,
    ) -> Self {
        let albedo = if albedo.is_finite() {
            albedo.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let direct_horizontal_lux = direct_normal_lux * elevation_rad.sin().max(0.0);
        let k = albedo / std::f32::consts::PI;
        Self {
            elevation_rad,
            direct_normal_lux,
            direct_horizontal_lux,
            diffuse_horizontal_lux,
            albedo,
            sunlit_cd_m2: k * (direct_horizontal_lux + diffuse_horizontal_lux),
            shade_cd_m2: k * diffuse_horizontal_lux,
        }
    }

    /// Shade over sun: the fraction of the card's light the sky alone gives.
    #[must_use]
    pub fn shade_ratio(&self) -> f32 {
        self.shade_cd_m2 / self.sunlit_cd_m2.max(1.0e-6)
    }
}

/// Where the default auto-exposure meter puts a sunlit grey card, as the exposure-relative
/// luminance the tonemapper receives (0.18 = middle grey), for a frame that is `sky_fraction`
/// clear sky of luminance `sky_cd_m2` and the rest ground of luminance `ground_cd_m2`, with the
/// card itself at `card_cd_m2`. Runs the renderer's own histogram meter (64 bins over
/// `[2⁻⁶, 2¹⁸)` cd/m², 40th–95th percentile log average) and middle-grey mapping.
#[must_use]
pub fn metered_card(card_cd_m2: f32, ground_cd_m2: f32, sky_cd_m2: f32, sky_fraction: f32) -> f32 {
    const BINS: usize = 64;
    const PIXELS: f32 = 100_000.0;
    let bin = |l: f32| {
        let t = (l.max(1.0e-6).log2() - crate::readback::HIST_LOG2_MIN) / crate::readback::HIST_LOG2_RANGE;
        ((t * BINS as f32).floor().max(0.0) as usize).min(BINS - 1)
    };
    let mut counts = [0u32; BINS];
    let sky = (sky_fraction.clamp(0.0, 1.0) * PIXELS).round() as u32;
    counts[bin(sky_cd_m2)] += sky;
    counts[bin(ground_cd_m2)] += PIXELS as u32 - sky;
    let average = crate::readback::average_log_luminance(
        &counts,
        crate::readback::METER_LOW_PERCENTILE,
        crate::readback::METER_HIGH_PERCENTILE,
    );
    let exposure = crate::post::exposure_from_ev100(crate::post::ev100_from_luminance(average));
    card_cd_m2 * exposure
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOWCASE_ELEVATION_DEG: f32 = 21.0;
    /// The showcase's clear morning air: aerosol optical depth 0.18 (its `sky.ron`), continental
    /// Ångström exponent 1.3 (`render.set_aerosol`).
    const SHOWCASE_AEROSOL_TURBIDITY: f32 = 0.18;

    fn luts_at(elevation_deg: f32) -> AtmosphereLuts {
        let el = elevation_deg.to_radians();
        let mut air = crate::AtmosphereParameters::earth();
        air.set_aerosol_turbidity(SHOWCASE_AEROSOL_TURBIDITY);
        air.set_aerosol_angstrom(crate::CONTINENTAL_ANGSTROM);
        crate::build_atmosphere_luts_for(&air, [0.0, el.sin(), el.cos()], 2.0)
    }

    /// Reported, not asserted: the default Earth air (aerosol optical depth 0.005, almost none)
    /// gives a sky about half as bright as the IES clear sky, so shade is about a seventh of sun.
    /// A map that wants realistic clear-day shadows sets `aerosol_turbidity` 0.1–0.2.
    #[test]
    fn clean_default_air_gives_dark_shade() {
        let el = SHOWCASE_ELEVATION_DEG.to_radians();
        let luts = crate::build_atmosphere_luts([0.0, el.sin(), el.cos()], 2.0);
        let card = GreyCard::engine(&luts, GREY_CARD_ALBEDO);
        println!(
            "default Earth air: {card:?}, shade / sun {:.3}",
            card.shade_ratio()
        );
        assert!(card.shade_ratio() > 0.0 && card.shade_ratio() < 1.0);
    }

    #[test]
    fn the_ies_model_gives_the_published_shape() {
        let (dn, kh) = ies_clear_sky(SHOWCASE_ELEVATION_DEG.to_radians());
        assert!((65_000.0..78_000.0).contains(&dn), "{dn}");
        assert!((9_000.0..11_000.0).contains(&kh), "{kh}");
        // Overhead sun: about 103 klx direct, 16 klx diffuse.
        let (dn90, kh90) = ies_clear_sky(std::f32::consts::FRAC_PI_2);
        assert!(dn90 > dn && kh90 > kh);
        assert_eq!(ies_clear_sky(-0.1), (0.0, 0.0));
        let card = GreyCard::ies(SHOWCASE_ELEVATION_DEG.to_radians(), GREY_CARD_ALBEDO);
        assert!((1_800.0..2_300.0).contains(&card.sunlit_cd_m2), "{card:?}");
        assert!((0.2..0.4).contains(&card.shade_ratio()), "{card:?}");
    }

    /// The grey-card calibration: under the engine's own sun and clear sky at the showcase's 21°
    /// sun, a 0.18 card on open ground shows the luminance the IES clear-sky model gives (within
    /// 30 %: the model is a fit to measured daylight and the engine's sky depends on its aerosol),
    /// its shade keeps at least a fifth of the sunlit value (the sky fills shadows), and a clear
    /// sky beside it is about as bright as the card, not an order of magnitude brighter.
    #[test]
    fn grey_card_under_the_21_degree_sun_matches_the_clear_sky_model() {
        let luts = luts_at(SHOWCASE_ELEVATION_DEG);
        let engine = GreyCard::engine(&luts, GREY_CARD_ALBEDO);
        let ies = GreyCard::ies(SHOWCASE_ELEVATION_DEG.to_radians(), GREY_CARD_ALBEDO);
        println!("engine {engine:?}");
        println!("ies    {ies:?}");
        let near = |a: f32, b: f32, tol: f32| (a / b - 1.0).abs() <= tol;
        assert!(
            near(engine.direct_normal_lux, ies.direct_normal_lux, 0.3),
            "direct normal {} vs IES {}",
            engine.direct_normal_lux,
            ies.direct_normal_lux
        );
        assert!(
            near(engine.diffuse_horizontal_lux, ies.diffuse_horizontal_lux, 0.4),
            "diffuse {} vs IES {}",
            engine.diffuse_horizontal_lux,
            ies.diffuse_horizontal_lux
        );
        assert!(
            near(engine.sunlit_cd_m2, ies.sunlit_cd_m2, 0.3),
            "sunlit card {} vs IES {}",
            engine.sunlit_cd_m2,
            ies.sunlit_cd_m2
        );
        assert!(engine.shade_ratio() > 0.2, "shade {:?}", engine.shade_ratio());
        // The sky 30° up, 90° in azimuth from the sun: within a factor of three of the card.
        let az = std::f32::consts::FRAC_PI_2;
        let el = 30f32.to_radians();
        let sky = luminance(crate::sample_sky_table(
            &luts,
            [el.cos() * az.sin(), el.sin(), el.cos() * az.cos()],
        ));
        println!(
            "sky 30° up, 90° from the sun: {sky} cd/m²; card / sky {}",
            engine.sunlit_cd_m2 / sky
        );
        assert!(
            (1.0 / 3.0..3.0).contains(&(engine.sunlit_cd_m2 / sky)),
            "card {} sky {sky}",
            engine.sunlit_cd_m2
        );
    }

    /// Where the meter puts the card: a frame of grey-card ground with up to a quarter sky (the
    /// s1 framing) lands the card within a stop of middle grey (0.09–0.36).
    #[test]
    fn the_meter_puts_a_sunlit_grey_card_within_a_stop_of_middle_grey() {
        let luts = luts_at(SHOWCASE_ELEVATION_DEG);
        let card = GreyCard::engine(&luts, GREY_CARD_ALBEDO);
        let az = std::f32::consts::FRAC_PI_2;
        let el = 20f32.to_radians();
        let sky = luminance(crate::sample_sky_table(
            &luts,
            [el.cos() * az.sin(), el.sin(), el.cos() * az.cos()],
        ));
        for sky_fraction in [0.0, 0.25] {
            let v = metered_card(card.sunlit_cd_m2, card.sunlit_cd_m2, sky, sky_fraction);
            println!(
                "sky {:.0} % of the frame: card at {v:.3} (0.18 = middle grey)",
                sky_fraction * 100.0
            );
            assert!((0.09..0.36).contains(&v), "{v} with {sky_fraction} sky");
        }
        // Reported, not asserted: with half the frame sky the 40th–95th percentile window is
        // mostly sky, so the meter exposes for it (ADR-1425 records the number).
        let half = metered_card(card.sunlit_cd_m2, card.sunlit_cd_m2, sky, 0.5);
        println!("sky 50 % of the frame: card at {half:.3}");
        // Ground only: the card is the frame average and lands on middle grey (one bin).
        let alone = metered_card(card.sunlit_cd_m2, card.sunlit_cd_m2, sky, 0.0);
        assert!((alone / 0.18 - 1.0).abs() < 0.3, "{alone}");
    }

    #[test]
    fn degenerate_inputs_stay_finite() {
        let card = GreyCard::ies(0.0, f32::NAN);
        assert_eq!(card.sunlit_cd_m2, 0.0);
        assert!(card.shade_ratio().is_finite());
        assert!(metered_card(0.0, 0.0, 0.0, 2.0).is_finite());
    }
}
