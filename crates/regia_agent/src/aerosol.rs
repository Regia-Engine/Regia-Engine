// SPDX-License-Identifier: MPL-2.0
//! `render.get_aerosol` / `render.set_aerosol`: the Ångström colour of the Earth haze
//! ([`AerosolSpectrum`], ADR-1408, book `agents/low-sun.md`, guide topic `aerosol`).

use regia_ecs::prelude::*;
use regia_render::{AEROSOL_WAVELENGTHS_NM, AerosolSpectrum, CONTINENTAL_ANGSTROM, angstrom_factors};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn aerosol_json(spectrum: Option<&AerosolSpectrum>) -> Value {
    let alpha = spectrum.map_or(0.0, AerosolSpectrum::alpha);
    json!({
        "active": spectrum.is_some(),
        "angstrom": alpha,
        "factors": angstrom_factors(alpha),
        "wavelengths_nm": AEROSOL_WAVELENGTHS_NM,
        "continental": CONTINENTAL_ANGSTROM,
    })
}

/// `render.get_aerosol`: whether an [`AerosolSpectrum`] is set, its exponent and the per-channel
/// Mie factors it applies.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    Ok(aerosol_json(world.get_resource::<AerosolSpectrum>()))
}

/// `render.set_aerosol { angstrom }`: inserts the resource (clamped to 0..3); `angstrom` null
/// removes it (flat Mie, as before).
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let before = aerosol_json(world.get_resource::<AerosolSpectrum>());
    match params.get("angstrom") {
        Some(Value::Null) => {
            world.remove_resource::<AerosolSpectrum>();
        }
        Some(v) => {
            let alpha = v
                .as_f64()
                .filter(|f| f.is_finite())
                .ok_or_else(|| RpcError::invalid_params("`angstrom` must be a finite number or null"))?;
            world.insert_resource(AerosolSpectrum {
                angstrom: (alpha as f32).clamp(0.0, 3.0),
            });
        }
        None => {
            return Err(RpcError::invalid_params(
                "`angstrom` is required (a number, or null to remove)",
            ));
        }
    }
    Ok(json!({ "before": before, "after": aerosol_json(world.get_resource::<AerosolSpectrum>()) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_inserts_clamps_and_removes() {
        let mut world = World::new();
        assert_eq!(get(&mut world).expect("get")["active"], false);
        let out = set(&mut world, &json!({ "angstrom": 1.3 })).expect("set");
        assert_eq!(out["after"]["active"], true);
        assert!(out["after"]["factors"][2].as_f64().unwrap_or(0.0) > 1.3);
        set(&mut world, &json!({ "angstrom": 8.0 })).expect("set");
        assert_eq!(
            world.get_resource::<AerosolSpectrum>().map(|a| a.angstrom),
            Some(3.0)
        );
        assert!(set(&mut world, &json!({ "angstrom": "blue" })).is_err());
        assert!(set(&mut world, &json!({})).is_err());
        set(&mut world, &json!({ "angstrom": null })).expect("remove");
        assert!(world.get_resource::<AerosolSpectrum>().is_none());
    }

    #[test]
    fn extreme_values_clamp_into_range_and_a_refused_call_changes_nothing() {
        let mut world = World::new();
        let angstrom = |w: &World| w.get_resource::<AerosolSpectrum>().map(|a| a.angstrom);
        set(&mut world, &json!({ "angstrom": -4.0 })).expect("negative");
        assert_eq!(angstrom(&world), Some(0.0));
        // 1e300 is infinite once narrowed to f32; it must clamp to the top, not become NaN.
        set(&mut world, &json!({ "angstrom": 1.0e300 })).expect("huge");
        assert_eq!(angstrom(&world), Some(3.0));
        set(&mut world, &json!({ "angstrom": -1.0e300 })).expect("huge negative");
        assert_eq!(angstrom(&world), Some(0.0));
        set(&mut world, &json!({ "angstrom": 1.3 })).expect("set");
        for bad in [
            json!({ "angstrom": true }),
            json!({ "angstrom": [1] }),
            json!({ "angstrom": {} }),
            json!({ "other": 1 }),
        ] {
            assert!(set(&mut world, &bad).is_err(), "{bad}");
            assert_eq!(angstrom(&world), Some(1.3), "{bad}");
        }
        // Setting twice gives the same state, and removing twice is fine.
        let a = set(&mut world, &json!({ "angstrom": 2.0 })).expect("a");
        let b = set(&mut world, &json!({ "angstrom": 2.0 })).expect("b");
        assert_eq!(a["after"], b["after"]);
        assert_eq!(b["before"], b["after"]);
        set(&mut world, &json!({ "angstrom": null })).expect("remove");
        let again = set(&mut world, &json!({ "angstrom": null })).expect("remove again");
        assert_eq!(again["before"]["active"], json!(false));
        assert_eq!(again["after"]["angstrom"], json!(0.0));
    }
}
