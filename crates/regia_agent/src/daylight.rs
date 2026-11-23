// SPDX-License-Identifier: MPL-2.0
//! `render.grey_card`: the daylight calibration of `regia_render::daylight` (ADR-1425, book
//! `agents/daylight-calibration.md`, guide topic `daylight`). A grey card on open ground under the
//! engine's Earth sun and clear sky for a sun elevation and an aerosol load, against the IES
//! clear-sky model, and where the auto-exposure meter puts it.

use regia_render::{AtmosphereParameters, GREY_CARD_ALBEDO, GreyCard, metered_card};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn number(params: &Value, key: &str, default: f32, lo: f32, hi: f32) -> Result<f32, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| (f as f32).clamp(lo, hi))
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a finite number"))),
    }
}

fn card_json(card: &GreyCard) -> Value {
    json!({
        "direct_normal_lux": card.direct_normal_lux,
        "direct_horizontal_lux": card.direct_horizontal_lux,
        "diffuse_horizontal_lux": card.diffuse_horizontal_lux,
        "sunlit_cd_m2": card.sunlit_cd_m2,
        "shade_cd_m2": card.shade_cd_m2,
        "shade_ratio": card.shade_ratio(),
    })
}

/// `render.grey_card { elevation_deg, aerosol_turbidity?, angstrom?, albedo?, sky_fraction? }`:
/// the card under the engine's sun and sky (`engine`) and under the IES clear-sky model (`ies`),
/// the clear sky 30° up and 90° in azimuth from the sun (`sky_cd_m2`), and the exposure-relative
/// value the default meter gives the sunlit card in a frame that is `sky_fraction` sky and the
/// rest ground of the card's brightness (`metered`, 0.18 = middle grey).
pub(crate) fn grey_card(params: &Value) -> Result<Value, RpcError> {
    let elevation_deg = match params.get("elevation_deg") {
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite() && *f > 0.0 && *f <= 90.0)
            .ok_or_else(|| RpcError::invalid_params("`elevation_deg` must be a number in (0, 90]"))?
            as f32,
        None => return Err(RpcError::invalid_params("`elevation_deg` is required")),
    };
    let turbidity = number(params, "aerosol_turbidity", 0.18, 0.0, 4.0)?;
    let angstrom = number(params, "angstrom", regia_render::CONTINENTAL_ANGSTROM, 0.0, 3.0)?;
    let albedo = number(params, "albedo", GREY_CARD_ALBEDO, 0.0, 1.0)?;
    let sky_fraction = number(params, "sky_fraction", 0.25, 0.0, 1.0)?;
    let el = elevation_deg.to_radians();
    let mut air = AtmosphereParameters::earth();
    air.set_aerosol_turbidity(turbidity);
    air.set_aerosol_angstrom(angstrom);
    let luts = regia_render::build_atmosphere_luts_for(&air, [0.0, el.sin(), el.cos()], 2.0);
    let engine = GreyCard::engine(&luts, albedo);
    let ies = GreyCard::ies(el, albedo);
    let (az, up) = (std::f32::consts::FRAC_PI_2, 30f32.to_radians());
    let sky = regia_render::daylight::luminance(regia_render::sample_sky_table(
        &luts,
        [up.cos() * az.sin(), up.sin(), up.cos() * az.cos()],
    ));
    Ok(json!({
        "elevation_deg": elevation_deg,
        "aerosol_turbidity": turbidity,
        "angstrom": angstrom,
        "albedo": albedo,
        "engine": card_json(&engine),
        "ies": card_json(&ies),
        "engine_over_ies": engine.sunlit_cd_m2 / ies.sunlit_cd_m2.max(1.0e-6),
        "sky_cd_m2": sky,
        "card_over_sky": engine.sunlit_cd_m2 / sky.max(1.0e-6),
        "sky_fraction": sky_fraction,
        "metered": metered_card(engine.sunlit_cd_m2, engine.sunlit_cd_m2, sky, sky_fraction),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_showcase_sun_gives_a_calibrated_card_and_bad_values_are_refused() {
        let out = grey_card(&json!({ "elevation_deg": 21.0 })).expect("card");
        let ratio = out["engine_over_ies"].as_f64().unwrap_or(0.0);
        assert!((0.7..1.3).contains(&ratio), "{out}");
        assert!(
            out["engine"]["shade_ratio"].as_f64().unwrap_or(0.0) > 0.2,
            "{out}"
        );
        let metered = out["metered"].as_f64().unwrap_or(0.0);
        assert!((0.09..0.36).contains(&metered), "{out}");
        assert!(grey_card(&json!({})).is_err());
        assert!(grey_card(&json!({ "elevation_deg": -5.0 })).is_err());
        assert!(grey_card(&json!({ "elevation_deg": 21.0, "albedo": "grey" })).is_err());
        // Clamped, not refused.
        let clamped = grey_card(&json!({ "elevation_deg": 21.0, "albedo": 7.0 })).expect("clamped");
        assert_eq!(clamped["albedo"], 1.0);
    }
}
