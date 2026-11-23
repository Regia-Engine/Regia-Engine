// SPDX-License-Identifier: MPL-2.0
//! Probe specular on the agent protocol (`book/agents/probe-specular.md`, ADR-1103).
//! `render.get_probe_specular` reads how the forward pass lights its indirect specular from the
//! probe light (traced probes and the baked volume), `render.set_probe_specular` switches it
//! between `"lobe"` (the default: the GGX lobe's mean vector at every roughness), `"rough_only"`
//! (the earlier look) and `"lobe_hue"` (the lobe keeping its hue and seeing the image's ground
//! plane, opt-in, ADR-2996), and `render.probe_specular_lobe` reports the fitted lobe for a roughness
//! and view angle against numerical integration, with the lobe's albedo, and the light a cube of
//! probe irradiances gives through it, so an agent can check the fit, the energy budget and the
//! mode without a frame.

use regia_app::prelude::*;
use regia_render::RuntimeRenderer;
use regia_render::probe_specular::{
    ProbeSpecular, cube_lobe_radiance, cube_lobe_radiance_hue, ggx_lobe_reference, specular_lobe,
    specular_lobe_vector,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

const MODES: &str = "\"lobe\" (probe light through the GGX lobe at every roughness, the default), \
                     \"rough_only\" (probe light only for rough surfaces, the earlier look) or \
                     \"lobe_hue\" (the lobe keeping its hue and seeing the image's ground plane)";

fn missing() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource: this app has no runtime renderer",
        "add regia_render::RenderPlugin (it inserts RuntimeRenderer)",
    )
}

fn state(world: &World) -> Result<Value, RpcError> {
    let runtime = world.get_resource::<RuntimeRenderer>().ok_or_else(missing)?;
    let ddgi = runtime.0.ddgi_settings().specular;
    let baked = runtime.0.baked_gi_settings().specular;
    Ok(json!({
        "mode": if ddgi == baked { ddgi.name() } else { "mixed" },
        "ddgi": ddgi.name(),
        "baked": baked.name(),
        "detail_max": regia_render::PROBE_SPECULAR_DETAIL_MAX,
        "hue_floor": regia_render::PROBE_SPECULAR_HUE_FLOOR,
        "baked_active": runtime.0.baked_gi_status().active,
        "ddgi_active": runtime.0.ddgi_active(),
    }))
}

/// `render.get_probe_specular`: the mode for the traced probes and for the baked volume.
pub(crate) fn get_probe_specular(world: &mut World) -> Result<Value, RpcError> {
    state(world)
}

/// `render.set_probe_specular`: `mode` = `"lobe"`, `"rough_only"` or `"lobe_hue"`, for the traced
/// probes and the baked volume both. Returns the state before and after.
pub(crate) fn set_probe_specular(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = match crate::params::opt_str(params, "mode")? {
        Some(name) => ProbeSpecular::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!("unknown probe specular mode {name:?}: use {MODES}"))
        })?,
        None => return Err(RpcError::invalid_params(format!("`mode` is required: {MODES}"))),
    };
    let before = state(world)?;
    {
        let mut runtime = world.get_resource_mut::<RuntimeRenderer>().ok_or_else(missing)?;
        let mut ddgi = runtime.0.ddgi_settings().clone();
        if ddgi.specular != mode {
            ddgi.specular = mode;
            runtime.0.set_ddgi_settings(ddgi);
        }
        let mut baked = runtime.0.baked_gi_settings();
        if baked.specular != mode {
            baked.specular = mode;
            runtime.0.set_baked_gi_settings(baked);
        }
    }
    let after = state(world)?;
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next frame wherever probe light covers the surface (traced \
                 probes or a baked volume); the probes themselves are unchanged. A level that \
                 installs its own baked volume restores the host's baked settings when it unloads",
    }))
}

fn unit_param(params: &Value, key: &str, default: Option<f64>) -> Result<f32, RpcError> {
    let v = match (params.get(key), default) {
        (Some(v), _) => v
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number in [0, 1]")))?,
        (None, Some(d)) => d,
        (None, None) => {
            return Err(RpcError::invalid_params(format!(
                "`{key}` is required (a number in [0, 1])"
            )));
        }
    };
    if !(0.0..=1.0).contains(&v) {
        return Err(RpcError::invalid_params(format!(
            "`{key}` must be in [0, 1], got {v}"
        )));
    }
    Ok(v as f32)
}

/// Six irradiances / π, +x, −x, +y, −y, +z, −z, each `[r, g, b]`, finite and not negative.
fn cube_param(value: &Value) -> Result<[[f32; 3]; 6], RpcError> {
    let bad = || {
        RpcError::invalid_params(
            "`cube` must be six [r, g, b] irradiances / pi (+x, -x, +y, -y, +z, -z), each finite and >= 0",
        )
    };
    let rows = value.as_array().filter(|rows| rows.len() == 6).ok_or_else(bad)?;
    let mut cube = [[0.0f32; 3]; 6];
    for (row, out) in rows.iter().zip(&mut cube) {
        let row = row.as_array().filter(|row| row.len() == 3).ok_or_else(bad)?;
        for (v, o) in row.iter().zip(out.iter_mut()) {
            *o = v
                .as_f64()
                .map(|v| v as f32)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(bad)?;
        }
    }
    Ok(cube)
}

/// The light `cube` gives through the lobe of mean vector `m` in `mode` (`mode` defaults to
/// `"lobe"`). `"rough_only"` has no lobe to read.
fn lobe_light(params: &Value, cube: &Value, m: [f32; 3]) -> Result<Value, RpcError> {
    let cube = cube_param(cube)?;
    let name = crate::params::opt_str(params, "mode")?.unwrap_or("lobe");
    let radiance = match ProbeSpecular::from_name(name) {
        Some(ProbeSpecular::Lobe) => cube_lobe_radiance(&cube, m),
        Some(ProbeSpecular::LobeHue) => cube_lobe_radiance_hue(&cube, m),
        Some(ProbeSpecular::RoughOnly) | None => {
            return Err(RpcError::invalid_params(format!(
                "`mode` {name:?} has no lobe light: use \"lobe\" or \"lobe_hue\""
            )));
        }
    };
    Ok(json!({ "mode": name, "radiance": radiance }))
}

/// `render.probe_specular_lobe`: the fitted GGX lobe for perceptual `roughness` and `n_dot_v`
/// (default 1) against numerical integration, and the lobe's albedo (≤ 1). With `cube`, also the
/// light those six irradiances / π give through the lobe's mean vector, along `direction` when
/// given (else as `fit`), in `mode`: the CPU mirror of the shader's lobe light.
pub(crate) fn probe_specular_lobe(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let perceptual = unit_param(params, "roughness", None)?;
    let n_dot_v = unit_param(params, "n_dot_v", Some(1.0))?.max(1e-3);
    let alpha = perceptual * perceptual;
    let lobe = specular_lobe(n_dot_v, alpha);
    let s = (1.0 - n_dot_v * n_dot_v).max(0.0).sqrt();
    let fit = specular_lobe_vector([0.0, 0.0, 1.0], [-s, 0.0, n_dot_v], n_dot_v, alpha);
    let reference = ggx_lobe_reference(n_dot_v, alpha);
    let error = fit
        .iter()
        .zip(reference.mean)
        .map(|(a, b)| (a - b) * (a - b))
        .sum::<f32>()
        .sqrt();
    let length = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    // `direction` and `mode` shape `light` alone, but are read without `cube` too: a wrong-typed
    // value is refused, never ignored (ADR-5960 (7), `typed_params_negative`).
    let lean = lobe_mean_along(params, lobe.length)?;
    crate::params::opt_str(params, "mode")?;
    let light = match params.get("cube") {
        Some(cube) => lobe_light(params, cube, lean.unwrap_or(fit))?,
        None => Value::Null,
    };
    Ok(json!({
        "roughness": perceptual,
        "alpha": alpha,
        "n_dot_v": n_dot_v,
        "lerp": lobe.lerp,
        "length": lobe.length,
        "fit": fit,
        "reference": {
            "mean": reference.mean,
            "length": length(reference.mean),
            "albedo": reference.albedo,
        },
        "error": error,
        "frame": "n = +z, v = (sqrt(1 - n_dot_v²), 0, n_dot_v); the mirror side has x < 0",
        "light": light,
    }))
}

/// `direction`, a world unit vector the lobe's mean lies along: the mean vector for a lobe of
/// length `len` there, or `None` without the parameter.
fn lobe_mean_along(params: &Value, len: f32) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(value) = params.get("direction") else {
        return Ok(None);
    };
    let bad = || RpcError::invalid_params("`direction` must be three finite numbers, not all 0");
    let v = value.as_array().filter(|v| v.len() == 3).ok_or_else(bad)?;
    let mut d = [0.0f32; 3];
    for (x, o) in v.iter().zip(&mut d) {
        *o = x
            .as_f64()
            .map(|x| x as f32)
            .filter(|x| x.is_finite())
            .ok_or_else(bad)?;
    }
    let norm = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if norm <= 1e-6 {
        return Err(bad());
    }
    Ok(Some(d.map(|x| x / norm * len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_both_probe_sources_and_reports_them() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        let got = get_probe_specular(&mut world).expect("get works");
        assert_eq!(got["mode"], "lobe", "the default is the lobe");
        assert_eq!(got["ddgi"], "lobe");
        assert_eq!(got["baked"], "lobe");
        let set = set_probe_specular(&mut world, &json!({ "mode": "rough_only" })).expect("set works");
        assert_eq!(set["before"]["mode"], "lobe");
        assert_eq!(set["after"]["mode"], "rough_only");
        let runtime = world.resource::<RuntimeRenderer>();
        assert_eq!(runtime.0.ddgi_settings().specular, ProbeSpecular::RoughOnly);
        assert_eq!(runtime.0.baked_gi_settings().specular, ProbeSpecular::RoughOnly);
        // A host that set only one source reads as mixed.
        let mut baked = world.resource::<RuntimeRenderer>().0.baked_gi_settings();
        baked.specular = ProbeSpecular::Lobe;
        world
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_baked_gi_settings(baked);
        assert_eq!(
            get_probe_specular(&mut world).expect("get works")["mode"],
            "mixed"
        );
        set_probe_specular(&mut world, &json!({ "mode": "lobe" })).expect("set works");
        assert_eq!(get_probe_specular(&mut world).expect("get works")["mode"], "lobe");
    }

    #[test]
    fn rejects_unknown_modes_and_missing_renderer() {
        let mut world = World::new();
        assert!(get_probe_specular(&mut world).is_err(), "no renderer");
        assert!(
            set_probe_specular(&mut world, &json!({ "mode": "lobe" })).is_err(),
            "no renderer"
        );
        world.insert_resource(RuntimeRenderer::default());
        for params in [json!({}), json!({ "mode": "mirror" }), json!({ "mode": 3 })] {
            let err = set_probe_specular(&mut world, &params).expect_err("bad mode");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{params}");
        }
    }

    #[test]
    fn the_hue_mode_is_reported_and_leaves_the_default_alone() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        let got = get_probe_specular(&mut world).expect("get works");
        assert_eq!(got["mode"], "lobe", "the default stays the lobe");
        assert!((got["hue_floor"].as_f64().expect("a number") - 0.2).abs() < 1e-6);
        let set = set_probe_specular(&mut world, &json!({ "mode": "lobe_hue" })).expect("set works");
        assert_eq!(set["before"]["mode"], "lobe");
        assert_eq!(set["after"]["mode"], "lobe_hue");
        let runtime = world.resource::<RuntimeRenderer>();
        assert_eq!(runtime.0.ddgi_settings().specular, ProbeSpecular::LobeHue);
        assert_eq!(runtime.0.baked_gi_settings().specular, ProbeSpecular::LobeHue);
    }

    /// A blue sky over a dim ground, the lobe straight down: the default's channels separate, the
    /// mode's do not, and the reply is the renderer's own mirror.
    #[test]
    fn the_lobe_light_of_a_cube_shows_the_mode() {
        let mut world = World::new();
        let side = [1557.6, 1997.1, 2861.0];
        let cube = json!([
            side,
            side,
            [2466.7, 3733.3, 6333.3],
            [600.0, 600.0, 600.0],
            side,
            side
        ]);
        let down = json!([0.0, -1.0, 0.0]);
        let read = |world: &mut World, mode: &str| {
            let params = json!({ "roughness": 0.0, "cube": cube, "direction": down, "mode": mode });
            let got = probe_specular_lobe(world, &params).expect("works");
            assert_eq!(got["light"]["mode"], mode);
            let rgb: Vec<f64> = got["light"]["radiance"]
                .as_array()
                .expect("radiance")
                .iter()
                .map(|v| v.as_f64().expect("number"))
                .collect();
            rgb
        };
        let lobe = read(&mut world, "lobe");
        assert!(lobe[2] == 0.0 && lobe[0] > 100.0, "the default's spot: {lobe:?}");
        let hue = read(&mut world, "lobe_hue");
        assert!(
            hue.iter().all(|c| *c > 250.0),
            "every channel keeps its share: {hue:?}"
        );
        // Without a cube there is no light to report; bad cubes and modes are refused.
        let plain = probe_specular_lobe(&mut world, &json!({ "roughness": 0.3 })).expect("works");
        assert!(plain["light"].is_null());
        for params in [
            json!({ "roughness": 0.3, "cube": [[1.0, 1.0, 1.0]] }),
            json!({ "roughness": 0.3, "cube": [[-1.0, 1.0, 1.0], side, side, side, side, side] }),
            json!({ "roughness": 0.3, "cube": cube, "mode": "rough_only" }),
            json!({ "roughness": 0.3, "cube": cube, "mode": "mirror" }),
            json!({ "roughness": 0.3, "cube": cube, "direction": [0.0, 0.0, 0.0] }),
        ] {
            let err = probe_specular_lobe(&mut world, &params).expect_err("bad params");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{params}");
        }
    }

    #[test]
    fn reports_the_fitted_lobe_against_integration() {
        let mut world = World::new();
        let mirror = probe_specular_lobe(&mut world, &json!({ "roughness": 0.0 })).expect("works");
        assert_eq!(mirror["lerp"], 1.0);
        assert_eq!(mirror["length"], 1.0);
        let rough =
            probe_specular_lobe(&mut world, &json!({ "roughness": 0.7, "n_dot_v": 0.5 })).expect("works");
        let error = rough["error"].as_f64().expect("error is a number");
        assert!(error < 0.04, "fit error {error}");
        let albedo = rough["reference"]["albedo"].as_f64().expect("albedo is a number");
        assert!(albedo > 0.0 && albedo <= 1.0, "albedo {albedo}");
        for params in [
            json!({}),
            json!({ "roughness": 1.5 }),
            json!({ "roughness": 0.5, "n_dot_v": -0.1 }),
        ] {
            let err = probe_specular_lobe(&mut world, &params).expect_err("bad params");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{params}");
        }
    }
}
